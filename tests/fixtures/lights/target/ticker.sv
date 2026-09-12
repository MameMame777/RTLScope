/// The timer's two wires, bundled: who may enable it, and who sees it tick.
///
/// Three sides: the timer's own, the controller's that says when to count,
/// and everyone else's, which watches and does not touch.
interface lights_Ticker;
    logic enable;
    logic tick  ;

    modport timer (
        input  enable,
        output tick  
    );

    modport user (
        output enable,
        input  tick  
    );

    modport watch (
        input enable,
        input tick  
    );
endinterface
//# sourceMappingURL=ticker.sv.map
