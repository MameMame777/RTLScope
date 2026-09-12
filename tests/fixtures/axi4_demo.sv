// AXI4 — the memory-mapped kind — small enough to read in one sitting.
//
// A design built around one question: what does an AXI4 transaction look like,
// cycle by cycle? A master writes a four-beat burst, reads it back, then reads
// an address the slave does not have, and every channel of the protocol is
// crossed on the way:
//
//   AW  write address    master -> slave   awaddr awlen awsize awburst   awvalid / awready
//   W   write data       master -> slave   wdata wstrb wlast             wvalid  / wready
//   B   write response   slave -> master   bresp                         bvalid  / bready
//   AR  read address     master -> slave   araddr arlen arsize arburst   arvalid / arready
//   R   read data        slave -> master   rdata rresp rlast             rvalid  / rready
//
// Every channel is the same handshake, and the whole protocol is four rules
// about it:
//
//   1. A transfer happens on the clock edge where VALID and READY are both high.
//   2. The source raises VALID as soon as it has something, and may not wait
//      for READY to do so. The sink may wait for VALID before raising READY.
//   3. Once VALID is high it stays high, with the same payload, until the
//      transfer. READY has no such rule: it may rise and fall as it likes.
//   4. A write is one AW, then AWLEN+1 beats of W with WLAST on the last, then
//      one B. A read is one AR, then ARLEN+1 beats of R with RLAST on the last,
//      and every R beat carries its own RRESP.
//
// The slave here is deliberately awkward, so the rules show in the recording.
// It holds AWREADY low for two cycles after AWVALID arrives (rule 2, from the
// sink's side). It accepts W only every other cycle, so WVALID can be seen
// staying up through the gaps (rule 3). It puts a bubble between R beats, and
// RVALID is a register that only a transfer clears (rule 3 again, from the
// other direction). And reading past its sixteen words answers SLVERR, which
// is what a response channel is for.
//
// Left out, as AXI4 allows: IDs (one master, so there is no ordering to keep),
// PROT, CACHE, QOS, REGION, LOCK and USER. A real interconnect carries them;
// the handshake is the same.
//
// Where to look, in the window:
//   the diagram   u_master and u_slave, with the five bundles of wire between them
//   FSM           axi4_master.state is the sequence, as states — one per channel
//                 crossed; the slave has a machine per direction
//   Wave          awvalid up while awready waits; wready pulsing under a steady
//                 wvalid; rvalid with a gap between beats; rresp = 2 at the end
//                 of each round; `done` for one cycle, then the round again
//   Source        click a signal name to trace it
//
// The recording beside this file was made with Verilator, which is the only
// simulator the window uses. Seventy cycles is two full rounds and the rest
// after the second, with the third not yet begun:
//   rtlscope sim axi4_demo tests/fixtures/axi4_demo.sv --cycles 70

// ---------------------------------------------------------------------------
// The master: a fixed programme, run after reset and again after every rest.
//
// It does not park when it is done. A master that stopped in a DONE state
// would be a testbench, not a master: the real thing takes a request, runs it
// to its response, and is idle again for the next one. Here the next request
// is the same programme, five cycles later, so a recording shows the return
// as well as the round.
//
// VALID and every payload signal are functions of the state alone, which is
// how rule 3 is kept without thinking about it: the state only moves on a
// transfer, so nothing the slave is looking at can change before one.
// ---------------------------------------------------------------------------
module axi4_master (
    input  logic        clk,
    input  logic        rst_n,
    // AW — where the write goes, and how long it is
    output logic [31:0] m_awaddr,
    output logic [7:0]  m_awlen,
    output logic [2:0]  m_awsize,
    output logic [1:0]  m_awburst,
    output logic        m_awvalid,
    input  logic        m_awready,
    // W — the data, one beat at a time
    output logic [31:0] m_wdata,
    output logic [3:0]  m_wstrb,
    output logic        m_wlast,
    output logic        m_wvalid,
    input  logic        m_wready,
    // B — how the write went
    input  logic [1:0]  m_bresp,
    input  logic        m_bvalid,
    output logic        m_bready,
    // AR — where to read, and how much
    output logic [31:0] m_araddr,
    output logic [7:0]  m_arlen,
    output logic [2:0]  m_arsize,
    output logic [1:0]  m_arburst,
    output logic        m_arvalid,
    input  logic        m_arready,
    // R — the data coming back, with a response on every beat
    input  logic [31:0] m_rdata,
    input  logic [1:0]  m_rresp,
    input  logic        m_rlast,
    input  logic        m_rvalid,
    output logic        m_rready,
    // What came of it
    output logic        done,
    output logic        pass,
    output logic [1:0]  write_resp,
    output logic [1:0]  read_resp
);

    localparam logic [31:0] BASE      = 32'h0000_0010;  // word 4 of the slave's sixteen
    localparam logic [31:0] BEYOND    = 32'h0000_0080;  // past the end, so: SLVERR
    localparam logic [7:0]  BURST_LEN = 8'd3;           // AxLEN is beats minus one: four beats
    localparam logic [2:0]  SIZE_4B   = 3'd2;           // AxSIZE is log2(bytes per beat)
    localparam logic [1:0]  INCR      = 2'b01;          // each beat at the next address
    localparam logic [31:0] PATTERN   = 32'hA5A5_0000;  // plus the beat number
    localparam logic [1:0]  RESP_OKAY   = 2'b00;
    localparam logic [1:0]  RESP_SLVERR = 2'b10;

    typedef enum logic [3:0] {
        M_IDLE   = 4'd0,   // a few quiet cycles, after reset and between rounds
        M_AW     = 4'd1,   // write address out, until the slave takes it
        M_W      = 4'd2,   // four beats of data
        M_B      = 4'd3,   // the one write response
        M_AR     = 4'd4,   // read address for the same four words
        M_R      = 4'd5,   // four beats back, checked against what was written
        M_AR_BAD = 4'd6,   // an address the slave does not have
        M_R_BAD  = 4'd7,   // the SLVERR it answers with
        M_DONE   = 4'd8    // one cycle of `done`, then back to the start
    } state_e;

    state_e     state, next_state;
    logic [7:0] beat;        // which beat of the burst in flight
    logic [3:0] settle;      // cycles idled so far, this rest
    logic       data_ok;     // every read beat of this round matched what was written
    logic       saw_slverr;  // the bad read of this round was refused, as it should be
    logic       pass_q;      // what the last finished round came to

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            state <= M_IDLE;
        else
            state <= next_state;
    end

    // Every move out of a channel state is a transfer on that channel: VALID
    // is up by construction in that state, so READY alone completes it.
    always_comb begin
        next_state = state;
        case (state)
            M_IDLE:   if (settle == 4'd4)          next_state = M_AW;
            M_AW:     if (m_awready)               next_state = M_W;
            M_W:      if (m_wready && m_wlast)     next_state = M_B;
            M_B:      if (m_bvalid)                next_state = M_AR;
            M_AR:     if (m_arready)               next_state = M_R;
            M_R:      if (m_rvalid && m_rlast)     next_state = M_AR_BAD;
            M_AR_BAD: if (m_arready)               next_state = M_R_BAD;
            M_R_BAD:  if (m_rvalid && m_rlast)     next_state = M_DONE;
            M_DONE:   next_state = M_IDLE;
            default:  next_state = M_IDLE;
        endcase
    end

    // Rule 2 and rule 3 at once: VALID is the state, and so is the payload.
    assign m_awvalid = (state == M_AW);
    assign m_awaddr  = BASE;
    assign m_awlen   = BURST_LEN;
    assign m_awsize  = SIZE_4B;
    assign m_awburst = INCR;

    assign m_wvalid  = (state == M_W);
    assign m_wdata   = PATTERN + {24'd0, beat};
    assign m_wstrb   = 4'hF;                  // every byte of every beat
    assign m_wlast   = (beat == BURST_LEN);

    assign m_bready  = (state == M_B);

    assign m_arvalid = (state == M_AR) || (state == M_AR_BAD);
    assign m_araddr  = (state == M_AR_BAD) ? BEYOND : BASE;
    assign m_arlen   = (state == M_AR_BAD) ? 8'd0 : BURST_LEN;
    assign m_arsize  = SIZE_4B;
    assign m_arburst = INCR;

    assign m_rready  = (state == M_R) || (state == M_R_BAD);

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            settle     <= '0;
            beat       <= '0;
            data_ok    <= 1'b1;
            saw_slverr <= 1'b0;
            pass_q     <= 1'b0;
            write_resp <= RESP_OKAY;
            read_resp  <= RESP_OKAY;
        end else begin
            // The rest is counted from zero each time, so the pause between
            // rounds is the same length as the one after reset; and the
            // round's verdict starts clean.
            if (state == M_IDLE) begin
                settle     <= settle + 4'd1;
                data_ok    <= 1'b1;
                saw_slverr <= 1'b0;
            end else begin
                settle <= '0;
            end

            if (state == M_DONE)
                pass_q <= data_ok && (write_resp == RESP_OKAY) && saw_slverr;

            // The beat count steps on a transfer, and starts over after the
            // last one so the next burst begins at zero.
            if (state == M_W && m_wready)
                beat <= m_wlast ? 8'd0 : beat + 8'd1;
            if (state == M_R && m_rvalid)
                beat <= m_rlast ? 8'd0 : beat + 8'd1;
            if (state == M_R_BAD && m_rvalid)
                beat <= 8'd0;

            if (state == M_B && m_bvalid)
                write_resp <= m_bresp;

            // Each beat back is compared with the beat that went out.
            if (state == M_R && m_rvalid) begin
                read_resp <= m_rresp;
                if (m_rdata != PATTERN + {24'd0, beat})
                    data_ok <= 1'b0;
            end
            if (state == M_R_BAD && m_rvalid) begin
                read_resp <= m_rresp;
                if (m_rresp == RESP_SLVERR)
                    saw_slverr <= 1'b1;
            end
        end
    end

    // `done` is one cycle wide, once per round. `pass` is what the last round
    // that finished came to — an OKAY for the write, the words back
    // unchanged, and a refusal for the address that is not there — and holds
    // while the next round runs.
    assign done = (state == M_DONE);
    assign pass = pass_q;

endmodule

// ---------------------------------------------------------------------------
// The slave: sixteen words of memory behind the protocol.
//
// Two machines, because AXI4 keeps the write and read halves independent — a
// read may be answered while a write is still in progress. Each machine is the
// life of one transaction: address, data, response.
// ---------------------------------------------------------------------------
module axi4_slave (
    input  logic        clk,
    input  logic        rst_n,
    // AW
    input  logic [31:0] s_awaddr,
    input  logic [7:0]  s_awlen,
    input  logic [2:0]  s_awsize,
    input  logic [1:0]  s_awburst,
    input  logic        s_awvalid,
    output logic        s_awready,
    // W
    input  logic [31:0] s_wdata,
    input  logic [3:0]  s_wstrb,
    input  logic        s_wlast,
    input  logic        s_wvalid,
    output logic        s_wready,
    // B
    output logic [1:0]  s_bresp,
    output logic        s_bvalid,
    input  logic        s_bready,
    // AR
    input  logic [31:0] s_araddr,
    input  logic [7:0]  s_arlen,
    input  logic [2:0]  s_arsize,
    input  logic [1:0]  s_arburst,
    input  logic        s_arvalid,
    output logic        s_arready,
    // R
    output logic [31:0] s_rdata,
    output logic [1:0]  s_rresp,
    output logic        s_rlast,
    output logic        s_rvalid,
    input  logic        s_rready
);

    localparam logic [1:0] RESP_OKAY   = 2'b00;
    localparam logic [1:0] RESP_SLVERR = 2'b10;
    localparam logic [1:0] INCR        = 2'b01;
    localparam logic [2:0] SIZE_4B     = 3'd2;

    // Sixteen words. The word is address bits [5:2]; an address with anything
    // above bit 5 set is not here, and says so on the response channel.
    logic [31:0] mem [0:15];

    // ---- the write side: AW, then W, then B, as one transaction ----

    typedef enum logic [1:0] {
        W_IDLE = 2'd0,   // waiting for an address
        W_DATA = 2'd1,   // taking beats until WLAST
        W_RESP = 2'd2    // holding BVALID until the master takes the response
    } wstate_e;

    wstate_e     wstate, wnext;
    logic [1:0]  aw_wait;   // cycles AWVALID has been seen; READY comes on the third
    logic [31:0] waddr;     // where the beat in hand goes, stepped per beat
    logic [7:0]  wlen;
    logic [7:0]  wbeat;
    logic        wfault;    // anything about this burst that cannot be honoured
    logic        pace;      // flips every cycle; W is taken on the odd ones

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            wstate <= W_IDLE;
        else
            wstate <= wnext;
    end

    always_comb begin
        wnext = wstate;
        case (wstate)
            W_IDLE:  if (s_awvalid && s_awready)           wnext = W_DATA;
            W_DATA:  if (s_wvalid && s_wready && s_wlast)  wnext = W_RESP;
            W_RESP:  if (s_bready)                         wnext = W_IDLE;
            default: wnext = W_IDLE;
        endcase
    end

    // READY may look at VALID, and may come and go: neither is a rule the
    // sink has to keep. AWREADY waits two cycles; WREADY pulses.
    assign s_awready = (wstate == W_IDLE) && (aw_wait == 2'd2);
    assign s_wready  = (wstate == W_DATA) && pace;
    assign s_bvalid  = (wstate == W_RESP);
    assign s_bresp   = wfault ? RESP_SLVERR : RESP_OKAY;

    // The byte lanes WSTRB names are written; the others keep what they had.
    logic        w_in_range;
    logic [3:0]  widx;
    logic [31:0] old_word;
    logic [31:0] merged;
    assign w_in_range = (waddr[31:6] == 26'd0);
    assign widx       = waddr[5:2];
    assign old_word   = mem[widx];
    assign merged     = {
        s_wstrb[3] ? s_wdata[31:24] : old_word[31:24],
        s_wstrb[2] ? s_wdata[23:16] : old_word[23:16],
        s_wstrb[1] ? s_wdata[15:8]  : old_word[15:8],
        s_wstrb[0] ? s_wdata[7:0]   : old_word[7:0]
    };

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            aw_wait <= '0;
            waddr   <= '0;
            wlen    <= '0;
            wbeat   <= '0;
            wfault  <= 1'b0;
            pace    <= 1'b0;
        end else begin
            pace <= ~pace;

            // Counting only while the master is waiting on us, so the wait is
            // measured from the moment AWVALID rose.
            if (wstate == W_IDLE && s_awvalid && !s_awready)
                aw_wait <= aw_wait + 2'd1;
            else
                aw_wait <= '0;

            if (wstate == W_IDLE && s_awvalid && s_awready) begin
                waddr  <= s_awaddr;
                wlen   <= s_awlen;
                wbeat  <= '0;
                // Only INCR bursts of whole words live here. Anything else is
                // accepted — the handshake must complete — and refused on B.
                wfault <= (s_awburst != INCR) || (s_awsize != SIZE_4B)
                       || (s_awaddr[31:6] != 26'd0);
            end

            if (wstate == W_DATA && s_wvalid && s_wready) begin
                if (w_in_range)
                    mem[widx] <= merged;
                else
                    wfault <= 1'b1;
                // A WLAST that disagrees with AWLEN is the master's mistake,
                // and B is where the slave gets to say so.
                if (s_wlast != (wbeat == wlen))
                    wfault <= 1'b1;
                waddr <= waddr + 32'd4;   // INCR: the next beat is the next word
                wbeat <= wbeat + 8'd1;
            end
        end
    end

    // ---- the read side: AR, then R beats ----

    typedef enum logic [1:0] {
        R_IDLE = 2'd0,   // waiting for an address
        R_DATA = 2'd1    // sending beats until RLAST is taken
    } rstate_e;

    rstate_e     rstate, rnext;
    logic        ar_wait;    // one cycle of ARVALID seen before ARREADY
    logic [31:0] raddr;
    logic [7:0]  rlen;
    logic [7:0]  rbeat;
    logic        rvalid_q;   // RVALID is a register: only a transfer clears it
    logic        rfault;
    logic        r_in_range;

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n)
            rstate <= R_IDLE;
        else
            rstate <= rnext;
    end

    always_comb begin
        rnext = rstate;
        case (rstate)
            R_IDLE:  if (s_arvalid && s_arready)           rnext = R_DATA;
            R_DATA:  if (s_rvalid && s_rready && s_rlast)  rnext = R_IDLE;
            default: rnext = R_IDLE;
        endcase
    end

    assign s_arready  = (rstate == R_IDLE) && ar_wait;
    assign r_in_range = (raddr[31:6] == 26'd0);
    assign s_rvalid   = rvalid_q;
    assign s_rdata    = (r_in_range && !rfault) ? mem[raddr[5:2]] : 32'hDEAD_BEEF;
    assign s_rresp    = (r_in_range && !rfault) ? RESP_OKAY : RESP_SLVERR;
    assign s_rlast    = (rbeat == rlen);

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            ar_wait  <= 1'b0;
            raddr    <= '0;
            rlen     <= '0;
            rbeat    <= '0;
            rvalid_q <= 1'b0;
            rfault   <= 1'b0;
        end else begin
            ar_wait <= (rstate == R_IDLE) && s_arvalid && !s_arready;

            if (rstate == R_IDLE && s_arvalid && s_arready) begin
                raddr  <= s_araddr;
                rlen   <= s_arlen;
                rbeat  <= '0;
                rfault <= (s_arburst != INCR) || (s_arsize != SIZE_4B);
            end

            // Raised when a beat is ready; dropped only by the transfer that
            // takes it, and left down for one cycle between beats so the gap
            // can be seen. A master that is slow to raise RREADY sees RVALID
            // hold, which is rule 3 from the slave's side.
            if (rstate == R_DATA) begin
                if (!rvalid_q)
                    rvalid_q <= 1'b1;
                else if (s_rready) begin
                    rvalid_q <= 1'b0;
                    raddr    <= raddr + 32'd4;
                    rbeat    <= rbeat + 8'd1;
                end
            end
        end
    end

endmodule

// ---------------------------------------------------------------------------
// The two of them, wired together. One wire per signal, named as the
// specification names them, so the bundles read in the diagram.
// ---------------------------------------------------------------------------
module axi4_demo (
    input  logic       clk,
    input  logic       rst_n,
    output logic       done,
    output logic       pass,
    output logic [1:0] write_resp,
    output logic [1:0] read_resp
);

    logic [31:0] axi_awaddr;
    logic [7:0]  axi_awlen;
    logic [2:0]  axi_awsize;
    logic [1:0]  axi_awburst;
    logic        axi_awvalid;
    logic        axi_awready;

    logic [31:0] axi_wdata;
    logic [3:0]  axi_wstrb;
    logic        axi_wlast;
    logic        axi_wvalid;
    logic        axi_wready;

    logic [1:0]  axi_bresp;
    logic        axi_bvalid;
    logic        axi_bready;

    logic [31:0] axi_araddr;
    logic [7:0]  axi_arlen;
    logic [2:0]  axi_arsize;
    logic [1:0]  axi_arburst;
    logic        axi_arvalid;
    logic        axi_arready;

    logic [31:0] axi_rdata;
    logic [1:0]  axi_rresp;
    logic        axi_rlast;
    logic        axi_rvalid;
    logic        axi_rready;

    axi4_master u_master (
        .clk        (clk),
        .rst_n      (rst_n),
        .m_awaddr   (axi_awaddr),
        .m_awlen    (axi_awlen),
        .m_awsize   (axi_awsize),
        .m_awburst  (axi_awburst),
        .m_awvalid  (axi_awvalid),
        .m_awready  (axi_awready),
        .m_wdata    (axi_wdata),
        .m_wstrb    (axi_wstrb),
        .m_wlast    (axi_wlast),
        .m_wvalid   (axi_wvalid),
        .m_wready   (axi_wready),
        .m_bresp    (axi_bresp),
        .m_bvalid   (axi_bvalid),
        .m_bready   (axi_bready),
        .m_araddr   (axi_araddr),
        .m_arlen    (axi_arlen),
        .m_arsize   (axi_arsize),
        .m_arburst  (axi_arburst),
        .m_arvalid  (axi_arvalid),
        .m_arready  (axi_arready),
        .m_rdata    (axi_rdata),
        .m_rresp    (axi_rresp),
        .m_rlast    (axi_rlast),
        .m_rvalid   (axi_rvalid),
        .m_rready   (axi_rready),
        .done       (done),
        .pass       (pass),
        .write_resp (write_resp),
        .read_resp  (read_resp)
    );

    axi4_slave u_slave (
        .clk        (clk),
        .rst_n      (rst_n),
        .s_awaddr   (axi_awaddr),
        .s_awlen    (axi_awlen),
        .s_awsize   (axi_awsize),
        .s_awburst  (axi_awburst),
        .s_awvalid  (axi_awvalid),
        .s_awready  (axi_awready),
        .s_wdata    (axi_wdata),
        .s_wstrb    (axi_wstrb),
        .s_wlast    (axi_wlast),
        .s_wvalid   (axi_wvalid),
        .s_wready   (axi_wready),
        .s_bresp    (axi_bresp),
        .s_bvalid   (axi_bvalid),
        .s_bready   (axi_bready),
        .s_araddr   (axi_araddr),
        .s_arlen    (axi_arlen),
        .s_arsize   (axi_arsize),
        .s_arburst  (axi_arburst),
        .s_arvalid  (axi_arvalid),
        .s_arready  (axi_arready),
        .s_rdata    (axi_rdata),
        .s_rresp    (axi_rresp),
        .s_rlast    (axi_rlast),
        .s_rvalid   (axi_rvalid),
        .s_rready   (axi_rready)
    );

endmodule
