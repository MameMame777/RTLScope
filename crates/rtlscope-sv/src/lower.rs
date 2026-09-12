//! Syntax tree to unresolved IR.
//!
//! Scope for this step is what `rtlscope dump-ports` needs: module headers,
//! parameters, ports, net declarations and instantiations. Process bodies and
//! `generate` unrolling arrive later; until then they become
//! [`UItem::Unsupported`] with the keyword the author wrote, so the gap shows up
//! in `--diag-format json` instead of as a module that quietly looks empty.
//!
//! Declarations are destructured through the typed AST so their order is
//! preserved. Leaves are found with `unwrap_node!`, which searches a subtree
//! rather than naming every intermediate production.

use std::path::{Path, PathBuf};

use rtlscope_ir::{
    BinOp, Diag, DiagCode, Diagnostics, PortDir, Span, UConn, UConns, UDesign, UExpr, UExprKind,
    UFunction, UFunctionArg, UIfacePort, UImport, UInstance, UInterface, UItem, UModport,
    UModportMember, UModule, UNet, UNetType, UPackage, UParam, UParamOverrides, UPort, UProcKind,
    URange, UStmt, UStmtKind,
};
use sv_parser::{self as sv, NodeEvent, RefNode, SyntaxTree, unwrap_node};

use crate::expr::ExprLower;
use crate::nav;
use crate::parse::{ParseError, ParseOptions, ParsedFile, parse_file};
use crate::source_map::SourceMap;

/// Parses and lowers a set of files into one unresolved design.
///
/// Files are independent: a file that fails to parse produces an error
/// diagnostic and the rest still load. `sv-parser` has no error recovery, so a
/// single syntax error costs the whole file — but not the whole run.
/// Stack for the thread that parses and lowers.
///
/// `sv-parser` is a recursive-descent parser, and the depth it reaches follows
/// the *expression* nesting rather than the file size. The main thread's stack
/// is not enough: on a real 41-file design, 25 of the files overflowed it
/// and aborted the process — no error to catch, the process simply dies — and
/// none of them used an exotic construct.
///
/// Measured on an 8 MB thread those same files parse, but a single expression
/// of 800 `+` terms still overflows while 400 survives. Real RTL sits between
/// those, so 8 MB is not headroom, it is luck. 256 MB is reserved address
/// space, not committed memory — untouched unless the parser descends that far
/// — so the margin costs nothing.
///
/// The thread wraps parsing *and* lowering rather than just the parse, because
/// a `SyntaxTree` cannot cross a thread boundary: it is nested deeply enough
/// that the compiler's own `Send` check overflows trying to prove that it could.
const PARSE_STACK: usize = 256 * 1024 * 1024;

pub fn lower_files(paths: &[PathBuf], options: &ParseOptions) -> (UDesign, Diagnostics) {
    let paths = paths.to_vec();
    let options = options.clone();

    std::thread::Builder::new()
        .name("rtlscope-parse".to_string())
        .stack_size(PARSE_STACK)
        .spawn(move || lower_files_on_this_thread(&paths, &options))
        .expect("spawning the parser thread")
        .join()
        .expect("the parser thread panicked")
}

fn lower_files_on_this_thread(paths: &[PathBuf], options: &ParseOptions) -> (UDesign, Diagnostics) {
    let mut lowerer = Lowerer::new();
    for path in paths {
        // A file that is not there is not a syntax error, and saying so saves
        // the reader looking for a typo that is not in the file.
        if !path.is_file() {
            lowerer.diags.push(Diag::error(
                DiagCode::FileUnreadable,
                format!("{}: no such file", path.display()),
            ));
            continue;
        }
        match parse_file(path, options) {
            Ok(parsed) => lowerer.add_file(&parsed),
            Err(error) => lowerer.add_parse_error(&error),
        }
    }
    lowerer.finish()
}

/// A written name only when it says something the read name does not.
fn differing(written: Option<String>, name: &str) -> Option<String> {
    written.filter(|written| written != name)
}

fn note_written_in(map: &SourceMap, items: &mut [UItem]) {
    for item in items {
        match item {
            UItem::Net { net } => net.written = differing(map.written_at(net.span), &net.name),
            UItem::Inst { inst } => inst.written = differing(map.written_at(inst.span), &inst.name),
            UItem::Param { param } => {
                param.written = differing(map.written_at(param.span), &param.name)
            }
            UItem::TypedefEnum { members, .. } => {
                for member in members {
                    member.written = differing(map.written_at(member.span), &member.name);
                }
            }
            UItem::TypedefStruct { members, .. } => {
                for member in members {
                    member.written = differing(map.written_at(member.span), &member.name);
                }
            }
            UItem::GenerateFor { body, .. } => note_written_in(map, body),
            UItem::GenerateIf { then_items, else_items, .. } => {
                note_written_in(map, then_items);
                note_written_in(map, else_items);
            }
            UItem::GenerateBlock { items, .. } => note_written_in(map, items),
            _ => {}
        }
    }
}

/// Puts a generic's arguments back on its written name.
///
/// Veryl writes one SystemVerilog module per way a generic module is used, and
/// names each after the whole: `Module55A::<Module55B>` becomes
/// `prj___Module55A__Module55B`. The written name found at the declaration is
/// `Module55A` for all of them, which would leave the reader three modules
/// with one name. The tail of the read name says which is which, so it is
/// spelled back the way the author would: `Module55A::<Module55B>`. Nested
/// arguments lose their own brackets — `Module55I::<Package55, 8, 16>` for
/// what was `Module55I::<Package55::<8, 16>>` — which is a name a reader can
/// still recognise, and a guess this does not make.
fn decode_generic(read: &str, written: String) -> String {
    let marker = format!("___{written}__");
    let Some(at) = read.find(&marker) else { return written };
    let args: Vec<&str> =
        read[at + marker.len()..].split("__").filter(|arg| !arg.is_empty()).collect();
    if args.is_empty() {
        return written;
    }
    format!("{written}::<{}>", args.join(", "))
}

#[derive(Default)]
pub struct Lowerer {
    pub(crate) map: SourceMap,
    pub(crate) diags: Diagnostics,
    modules: Vec<UModule>,
    packages: Vec<UPackage>,
    interfaces: Vec<UInterface>,
    /// The `import` lines met while lowering the current module or package,
    /// wherever they were written in it. Taken when the declaration is done.
    pending_imports: Vec<UImport>,
    /// Where the editable constructs sit, for writing patches later.
    /// The function whose body is being lowered, if any.
    ///
    /// `return expr;` assigns to it: SystemVerilog gives a function's result
    /// the function's own name, and `return` is the other way to write that.
    pub(crate) fn_name: Option<String>,
    /// The flag standing in for "this function has already returned", when the
    /// body has a `return` that leaves before the end.
    ///
    /// `return` is control flow, and the hardware equivalent is a signal that
    /// says whether the result is settled: every statement after the return is
    /// guarded by it. `None` when the only `return` is the last statement, in
    /// which case there is nothing to guard and no flag to build.
    pub(crate) fn_flag: Option<String>,
}

impl Lowerer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn finish(mut self) -> (UDesign, Diagnostics) {
        self.note_written_names();
        let (files, generated) = self.map.into_parts();
        let design = UDesign {
            modules: self.modules,
            packages: self.packages,
            interfaces: self.interfaces,
            files,
            generated,
        };
        (design, self.diags)
    }

    /// Writes down, beside every name read from a generated file, the name the
    /// author gave it.
    ///
    /// One pass at the end rather than at each construction, because every
    /// named thing already carries a span, and the span already points at the
    /// author's own spelling — the map put it there. A name that comes back
    /// the same is not written down: the common case is no rename at all.
    fn note_written_names(&mut self) {
        let map = &self.map;
        for module in &mut self.modules {
            module.written = map
                .written_after_keyword(module.span)
                .filter(|written| *written != module.name)
                .map(|written| decode_generic(&module.name, written));
            for param in &mut module.params {
                param.written = differing(map.written_at(param.span), &param.name);
            }
            for port in &mut module.ports {
                port.written = differing(map.written_at(port.span), &port.name);
            }
            note_written_in(map, &mut module.items);
        }
        for package in &mut self.packages {
            package.written = map
                .written_after_keyword(package.span)
                .filter(|written| *written != package.name)
                .map(|written| decode_generic(&package.name, written));
            note_written_in(map, &mut package.items);
        }
        for interface in &mut self.interfaces {
            interface.written = map
                .written_after_keyword(interface.span)
                .filter(|written| *written != interface.name)
                .map(|written| decode_generic(&interface.name, written));
            for param in &mut interface.params {
                param.written = differing(map.written_at(param.span), &param.name);
            }
            for port in &mut interface.ports {
                port.written = differing(map.written_at(port.span), &port.name);
            }
            note_written_in(map, &mut interface.items);
        }
    }

    /// Records a file that could not be parsed, pointing at the offending line
    /// when `sv-parser` knew where it was.
    pub fn add_parse_error(&mut self, error: &ParseError) {
        let mut diag = Diag::error(DiagCode::ParseFailed, error.to_string());
        if let Some((path, offset)) = error.location() {
            let span = self.map.resolve_offset(path, offset, 1);
            if !span.is_unknown() {
                diag = diag.at(span);
            }
        }
        self.diags.push(diag);
    }

    pub fn add_file(&mut self, parsed: &ParsedFile) {
        let tree = &parsed.tree;
        // An `import` outside any declaration is for every module in the file
        // after it. Depth is counted so the ones inside a module are left to
        // the module, which meets them itself.
        let mut file_imports: Vec<UImport> = Vec::new();
        let mut depth = 0usize;
        for event in tree.into_iter().event() {
            match event {
                NodeEvent::Enter(RefNode::ModuleDeclaration(declaration)) => {
                    if depth == 0 {
                        self.module_declaration(tree, declaration, &parsed.path, &file_imports);
                    }
                    depth += 1;
                }
                NodeEvent::Enter(RefNode::PackageDeclaration(declaration)) => {
                    if depth == 0 {
                        self.package_declaration(tree, declaration, &file_imports);
                    }
                    depth += 1;
                }
                NodeEvent::Enter(RefNode::InterfaceDeclaration(declaration)) => {
                    if depth == 0 {
                        self.interface_declaration(tree, declaration, &parsed.path, &file_imports);
                    }
                    depth += 1;
                }
                NodeEvent::Leave(RefNode::ModuleDeclaration(_))
                | NodeEvent::Leave(RefNode::PackageDeclaration(_))
                | NodeEvent::Leave(RefNode::InterfaceDeclaration(_)) => {
                    depth = depth.saturating_sub(1);
                }
                NodeEvent::Enter(RefNode::PackageImportDeclaration(import)) if depth == 0 => {
                    self.package_import(tree, import);
                    file_imports.append(&mut self.pending_imports);
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------ helpers ---

    pub(crate) fn span(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> Span {
        match nav::first_token(node) {
            Some(locate) => self.map.resolve(tree, &locate),
            None => Span::UNKNOWN,
        }
    }

    pub(crate) fn expr<'t>(&mut self, tree: &'t SyntaxTree) -> ExprLower<'t, '_> {
        ExprLower { tree, map: &mut self.map }
    }

    pub(crate) fn keyword_of(&self, tree: &SyntaxTree, node: RefNode<'_>) -> String {
        nav::first_token(node)
            .and_then(|locate| tree.get_str(&locate).map(str::to_owned))
            .unwrap_or_else(|| "<construct>".to_string())
    }

    pub(crate) fn unsupported_item(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> UItem {
        let construct = self.keyword_of(tree, node.clone());
        let span = self.span(tree, node);
        self.diags.push(
            Diag::warning(
                DiagCode::UnsupportedConstruct,
                format!("`{construct}` is outside the synthesisable subset and was skipped"),
            )
            .at(span),
        );
        UItem::Unsupported { construct, span }
    }

    // ------------------------------------------------------------ modules ---

    fn module_declaration(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::ModuleDeclaration,
        path: &Path,
        file_imports: &[UImport],
    ) {
        let mut module = match declaration {
            sv::ModuleDeclaration::Ansi(ansi) => {
                let (header, _timeunits, items, _end, _label) = &ansi.nodes;
                let mut module = self.ansi_header(tree, header);
                for item in items {
                    let lowered = self.non_port_module_item(tree, item);
                    module.items.extend(lowered);
                }
                module
            }
            sv::ModuleDeclaration::Nonansi(nonansi) => {
                let (header, _timeunits, items, _end, _label) = &nonansi.nodes;
                let mut module = self.nonansi_header(tree, header);
                for item in items {
                    let lowered = self.module_item(tree, item);
                    module.items.extend(lowered);
                }
                module
            }
            other => {
                let span = self.span(tree, RefNode::ModuleDeclaration(other));
                self.diags.push(
                    Diag::warning(
                        DiagCode::UnsupportedConstruct,
                        format!(
                            "{}: module declaration form is outside the subset",
                            path.display()
                        ),
                    )
                    .at(span),
                );
                return;
            }
        };

        // The whole declaration, down to `endmodule`, because that is what an
        // edit has to reach: adding a port or an instance is an insertion
        // inside this range and nowhere else.

        module.imports.extend(file_imports.iter().cloned());
        module.imports.append(&mut self.pending_imports);

        if self.modules.iter().any(|m| m.name == module.name) {
            self.diags.push(
                Diag::warning(
                    DiagCode::DuplicateModule,
                    format!("module `{}` is declared more than once", module.name),
                )
                .at(module.span),
            );
        }
        self.modules.push(module);
    }

    fn ansi_header(&mut self, tree: &SyntaxTree, header: &sv::ModuleAnsiHeader) -> UModule {
        let (_attrs, _keyword, _lifetime, identifier, imports, parameters, ports, _semi) =
            &header.nodes;
        // `module m import defs::*; (...)`: the imports come before the
        // parameters and ports, which may use what they bring in.
        for import in imports {
            self.package_import(tree, import);
        }

        let name = nav::identifier_str(tree, RefNode::ModuleIdentifier(identifier))
            .unwrap_or_else(|| "<anonymous>".to_string());
        let span = self.span(tree, RefNode::ModuleAnsiHeader(header));
        let params = parameters
            .as_ref()
            .map(|list| self.parameter_port_list(tree, list))
            .unwrap_or_default();
        let ports = ports.as_ref().map(|list| self.ansi_ports(tree, list)).unwrap_or_default();

        UModule {
            name,
            written: None,
            params,
            ports,
            items: Vec::new(),
            ansi_header: true,
            imports: Vec::new(),
            span,
        }
    }

    fn nonansi_header(&mut self, tree: &SyntaxTree, header: &sv::ModuleNonansiHeader) -> UModule {
        let (_attrs, _keyword, _lifetime, identifier, imports, parameters, ports, _semi) =
            &header.nodes;
        for import in imports {
            self.package_import(tree, import);
        }

        let name = nav::identifier_str(tree, RefNode::ModuleIdentifier(identifier))
            .unwrap_or_else(|| "<anonymous>".to_string());
        let span = self.span(tree, RefNode::ModuleNonansiHeader(header));
        let params = parameters
            .as_ref()
            .map(|list| self.parameter_port_list(tree, list))
            .unwrap_or_default();

        // A non-ANSI header lists bare names; direction and type arrive later as
        // body declarations, which elaboration merges in.
        let (_open, list, _close) = &ports.nodes.0.nodes;
        let mut lowered = Vec::new();
        for port in list.contents() {
            let Some(name) = nav::identifier_str(tree, RefNode::Port(port)) else { continue };
            let span = self.span(tree, RefNode::Port(port));
            lowered.push(UPort {
                name,
                written: None,
                dir: None,
                net_type: None,
                type_name: None,
                packed: None,
                unpacked: None,
                iface: None,
                span,
            });
        }

        UModule {
            name,
            written: None,
            params,
            ports: lowered,
            items: Vec::new(),
            ansi_header: false,
            imports: Vec::new(),
            span,
        }
    }

    // ---------------------------------------------------------- interfaces ---

    /// `interface bus_if #(parameter W = 8) (input clk); ... modport slave
    /// (...); endinterface`
    ///
    /// Read like a module: the same header, the same body dispatch. What is
    /// kept is what elaboration unfolds — the signals, the logic among them,
    /// and the modports that give a port of this interface its directions.
    fn interface_declaration(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::InterfaceDeclaration,
        path: &Path,
        file_imports: &[UImport],
    ) {
        let sv::InterfaceDeclaration::Ansi(ansi) = declaration else {
            let span = self.span(tree, RefNode::InterfaceDeclaration(declaration));
            self.diags.push(
                Diag::warning(
                    DiagCode::UnsupportedConstruct,
                    format!(
                        "{}: only an interface with an ANSI header is read; this one is skipped",
                        path.display()
                    ),
                )
                .at(span),
            );
            return;
        };
        let (header, _timeunits, items, _end, _label) = &ansi.nodes;
        let (_attrs, _keyword, _lifetime, identifier, imports, parameters, ports, _semi) =
            &header.nodes;
        for import in imports {
            self.package_import(tree, import);
        }
        let name = nav::identifier_str(tree, RefNode::InterfaceIdentifier(identifier))
            .unwrap_or_else(|| "<anonymous>".to_string());
        let span = self.span(tree, RefNode::InterfaceAnsiHeader(header));
        let params = parameters
            .as_ref()
            .map(|list| self.parameter_port_list(tree, list))
            .unwrap_or_default();
        let ports = ports.as_ref().map(|list| self.ansi_ports(tree, list)).unwrap_or_default();

        let mut lowered = Vec::new();
        let mut modports = Vec::new();
        for item in items {
            match item {
                sv::NonPortInterfaceItem::GenerateRegion(region) => {
                    let (_keyword, inner, _end) = &region.nodes;
                    lowered.extend(self.generate_items(tree, inner));
                }
                sv::NonPortInterfaceItem::InterfaceOrGenerateItem(inner) => match &**inner {
                    sv::InterfaceOrGenerateItem::Module(common) => {
                        let node = RefNode::ModuleCommonItem(&common.nodes.1);
                        lowered.extend(self.module_or_generate_item(tree, node));
                    }
                    sv::InterfaceOrGenerateItem::Extern(x) => {
                        let node = RefNode::InterfaceOrGenerateItemExtern(x);
                        lowered.push(self.unsupported_item(tree, node));
                    }
                },
                sv::NonPortInterfaceItem::ModportDeclaration(declaration) => {
                    modports.extend(self.modport_declaration(tree, declaration));
                }
                sv::NonPortInterfaceItem::TimeunitsDeclaration(_) => {}
                other => {
                    lowered.push(self.unsupported_item(tree, RefNode::NonPortInterfaceItem(other)))
                }
            }
        }

        let mut imports = file_imports.to_vec();
        imports.append(&mut self.pending_imports);

        if self.interfaces.iter().any(|i| i.name == name) {
            self.diags.push(
                Diag::warning(
                    DiagCode::DuplicateModule,
                    format!("interface `{name}` is declared more than once"),
                )
                .at(span),
            );
        }
        self.interfaces.push(UInterface {
            name,
            written: None,
            params,
            ports,
            imports,
            items: lowered,
            modports,
            span,
        });
    }

    /// `modport slave (input data, output ready, import get);`
    ///
    /// The signals and their directions. A function a modport imports is not
    /// a signal and not hardware until it is called, so it is passed over
    /// without a word; a clocking block or a `.name(expr)` port is reported.
    fn modport_declaration(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::ModportDeclaration,
    ) -> Vec<UModport> {
        let (_keyword, items, _semi) = &declaration.nodes;
        let mut out = Vec::new();
        for item in items.contents() {
            let (identifier, paren) = &item.nodes;
            let Some(name) = nav::identifier_str(tree, RefNode::ModportIdentifier(identifier))
            else {
                continue;
            };
            let span = self.span(tree, RefNode::ModportItem(item));
            let mut members = Vec::new();
            for ports in paren.nodes.1.contents() {
                match ports {
                    sv::ModportPortsDeclaration::Simple(simple) => {
                        let (_attrs, declaration) = &simple.nodes;
                        let (direction, list) = &declaration.nodes;
                        let Some(dir) = port_direction(tree, RefNode::PortDirection(direction))
                        else {
                            continue;
                        };
                        for port in list.contents() {
                            match port {
                                sv::ModportSimplePort::Ordered(ordered) => {
                                    let node = RefNode::PortIdentifier(&ordered.nodes.0);
                                    if let Some(member) = nav::identifier_str(tree, node.clone()) {
                                        let span = self.span(tree, node);
                                        members.push(UModportMember { name: member, dir, span });
                                    }
                                }
                                sv::ModportSimplePort::Named(named) => {
                                    let node = RefNode::ModportSimplePortNamed(named);
                                    self.unsupported_item(tree, node);
                                }
                            }
                        }
                    }
                    sv::ModportPortsDeclaration::Tf(_) => {}
                    sv::ModportPortsDeclaration::Clocking(clocking) => {
                        let node = RefNode::ModportPortsDeclarationClocking(clocking);
                        self.unsupported_item(tree, node);
                    }
                }
            }
            out.push(UModport { name, members, span });
        }
        out
    }

    /// What an interface port is a port of, from its header.
    fn interface_port_header(
        &self,
        tree: &SyntaxTree,
        header: &sv::InterfacePortHeader,
    ) -> UIfacePort {
        let modport_of = |modport: &Option<(sv::Symbol, sv::ModportIdentifier)>| {
            modport.as_ref().and_then(|(_dot, modport)| {
                nav::identifier_str(tree, RefNode::ModportIdentifier(modport))
            })
        };
        match header {
            sv::InterfacePortHeader::Identifier(x) => {
                let (interface, modport) = &x.nodes;
                UIfacePort {
                    interface: nav::identifier_str(tree, RefNode::InterfaceIdentifier(interface)),
                    modport: modport_of(modport),
                }
            }
            sv::InterfacePortHeader::Interface(x) => {
                let (_keyword, modport) = &x.nodes;
                UIfacePort { interface: None, modport: modport_of(modport) }
            }
        }
    }

    // ------------------------------------------------------------ packages ---

    /// `package defs; ... endpackage`
    ///
    /// The items are lowered by the same dispatch as a module body: a package
    /// holds parameters, typedefs and functions, which are the declarations a
    /// module holds too, and whatever else is in one is reported the same way.
    /// A function is marked with the package it came from, because its body
    /// names the package's constants bare and a caller may never have imported
    /// them.
    fn package_declaration(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::PackageDeclaration,
        file_imports: &[UImport],
    ) {
        let (_attrs, _keyword, _lifetime, identifier, _semi, _timeunits, items, _end, _label) =
            &declaration.nodes;
        let name = nav::identifier_str(tree, RefNode::PackageIdentifier(identifier))
            .unwrap_or_else(|| "<anonymous>".to_string());
        let span = self.span(tree, RefNode::PackageDeclaration(declaration));

        let mut lowered = Vec::new();
        for (_attrs, item) in items {
            match item {
                sv::PackageItem::PackageOrGenerateItemDeclaration(inner) => {
                    let node = RefNode::PackageOrGenerateItemDeclaration(inner);
                    lowered.extend(self.module_or_generate_item(tree, node));
                }
                // An `export` re-publishes names this pass does not track, and
                // a timeunit says nothing about them; neither is a gap worth a
                // warning.
                sv::PackageItem::PackageExportDeclaration(_)
                | sv::PackageItem::TimeunitsDeclaration(_) => {}
                sv::PackageItem::AnonymousProgram(program) => {
                    lowered.push(self.unsupported_item(tree, RefNode::AnonymousProgram(program)));
                }
            }
        }
        for item in &mut lowered {
            if let UItem::Function { func } = item {
                func.package = Some(name.clone());
            }
        }

        let mut imports = file_imports.to_vec();
        imports.append(&mut self.pending_imports);

        if self.packages.iter().any(|p| p.name == name) {
            self.diags.push(
                Diag::warning(
                    DiagCode::DuplicateModule,
                    format!("package `{name}` is declared more than once"),
                )
                .at(span),
            );
        }
        self.packages.push(UPackage { name, written: None, imports, items: lowered, span });
    }

    /// `import defs::*;` or `import defs::WIDTH, defs::mode_t;`, wherever it
    /// sits: each item is kept for the module or package being lowered.
    fn package_import(&mut self, tree: &SyntaxTree, import: &sv::PackageImportDeclaration) {
        let (_keyword, list, _semi) = &import.nodes;
        for item in list.contents() {
            let span = self.span(tree, RefNode::PackageImportItem(item));
            let (package, name) = match item {
                sv::PackageImportItem::Identifier(x) => {
                    let (package, _colons, identifier) = &x.nodes;
                    (
                        nav::identifier_str(tree, RefNode::PackageIdentifier(package)),
                        nav::identifier_str(tree, RefNode::Identifier(identifier)),
                    )
                }
                sv::PackageImportItem::Asterisk(x) => {
                    let (package, _colons, _star) = &x.nodes;
                    (nav::identifier_str(tree, RefNode::PackageIdentifier(package)), None)
                }
            };
            if let Some(package) = package {
                self.pending_imports.push(UImport { package, item: name, span });
            }
        }
    }

    // --------------------------------------------------------- parameters ---

    fn parameter_port_list(
        &mut self,
        tree: &SyntaxTree,
        list: &sv::ParameterPortList,
    ) -> Vec<UParam> {
        let mut out = Vec::new();
        match list {
            sv::ParameterPortList::Declaration(declaration) => {
                let (_hash, paren) = &declaration.nodes;
                for entry in paren.nodes.1.contents() {
                    self.parameter_port_declaration(tree, entry, &mut out);
                }
            }
            sv::ParameterPortList::Assignment(assignment) => {
                let (_hash, paren) = &assignment.nodes;
                let (first, rest) = &paren.nodes.1;
                self.param_assignments(tree, first, false, None, &mut out);
                for (_comma, entry) in rest {
                    self.parameter_port_declaration(tree, entry, &mut out);
                }
            }
            sv::ParameterPortList::Empty(_) => {}
        }
        out
    }

    fn parameter_port_declaration(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::ParameterPortDeclaration,
        out: &mut Vec<UParam>,
    ) {
        match declaration {
            sv::ParameterPortDeclaration::ParameterDeclaration(p) => {
                self.parameter_declaration(tree, p, false, out)
            }
            sv::ParameterPortDeclaration::LocalParameterDeclaration(p) => {
                self.local_parameter_declaration(tree, p, out)
            }
            sv::ParameterPortDeclaration::ParamList(p) => {
                let (data_type, assignments) = &p.nodes;
                let packed = self.packed_range(tree, RefNode::DataType(data_type));
                self.param_assignments(tree, assignments, false, packed, out);
            }
            sv::ParameterPortDeclaration::TypeList(t) => {
                let item =
                    self.unsupported_item(tree, RefNode::ParameterPortDeclarationTypeList(t));
                let _ = item;
            }
        }
    }

    fn parameter_declaration(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::ParameterDeclaration,
        is_local: bool,
        out: &mut Vec<UParam>,
    ) {
        match declaration {
            sv::ParameterDeclaration::Param(p) => {
                let (_keyword, data_type, assignments) = &p.nodes;
                let packed = self.packed_range(tree, RefNode::DataTypeOrImplicit(data_type));
                self.param_assignments(tree, assignments, is_local, packed, out);
            }
            sv::ParameterDeclaration::Type(t) => {
                self.unsupported_item(tree, RefNode::ParameterDeclarationType(t));
            }
        }
    }

    fn local_parameter_declaration(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::LocalParameterDeclaration,
        out: &mut Vec<UParam>,
    ) {
        match declaration {
            sv::LocalParameterDeclaration::Param(p) => {
                let (_keyword, data_type, assignments) = &p.nodes;
                let packed = self.packed_range(tree, RefNode::DataTypeOrImplicit(data_type));
                self.param_assignments(tree, assignments, true, packed, out);
            }
            sv::LocalParameterDeclaration::Type(t) => {
                self.unsupported_item(tree, RefNode::LocalParameterDeclarationType(t));
            }
        }
    }

    fn param_assignments(
        &mut self,
        tree: &SyntaxTree,
        assignments: &sv::ListOfParamAssignments,
        is_local: bool,
        packed: Option<URange>,
        out: &mut Vec<UParam>,
    ) {
        for assignment in assignments.nodes.0.contents() {
            let (identifier, _dimensions, value) = &assignment.nodes;
            let Some(name) = nav::identifier_str(tree, RefNode::ParameterIdentifier(identifier))
            else {
                continue;
            };
            let span = self.span(tree, RefNode::ParamAssignment(assignment));
            let default = value
                .as_ref()
                .and_then(|(_eq, expression)| self.constant_param_expression(tree, expression));
            out.push(UParam {
                name,
                written: None,
                is_local,
                packed: packed.clone(),
                default,
                span,
            });
        }
    }

    fn constant_param_expression(
        &mut self,
        tree: &SyntaxTree,
        expression: &sv::ConstantParamExpression,
    ) -> Option<UExpr> {
        match expression {
            sv::ConstantParamExpression::ConstantMintypmaxExpression(m) => match &**m {
                sv::ConstantMintypmaxExpression::Unary(e) => {
                    Some(self.expr(tree).constant_expression(e))
                }
                sv::ConstantMintypmaxExpression::Ternary(_) => None,
            },
            // `parameter type T = int` and `parameter p = $` are both outside
            // the integer-only subset.
            _ => None,
        }
    }

    // -------------------------------------------------------------- ports ---

    fn ansi_ports(&mut self, tree: &SyntaxTree, list: &sv::ListOfPortDeclarations) -> Vec<UPort> {
        let (_open, entries, _close) = &list.nodes.0.nodes;
        let Some(entries) = entries else { return Vec::new() };

        let mut out: Vec<UPort> = Vec::new();
        for (_attrs, declaration) in entries.contents() {
            // A port that omits its header inherits the previous port's
            // direction *and* its type, per IEEE 1800 §23.2.2.2-3: in
            // `input logic [7:0] a, b`, `b` is also eight bits. Inheriting the
            // direction alone would leave it silently one bit wide.
            let inherited = out.last().cloned();
            if let Some(port) = self.ansi_port(tree, declaration, inherited.as_ref()) {
                out.push(port);
            }
        }
        out
    }

    fn ansi_port(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::AnsiPortDeclaration,
        inherited: Option<&UPort>,
    ) -> Option<UPort> {
        let (header_node, identifier, dimensions) = match declaration {
            sv::AnsiPortDeclaration::Net(net) => {
                let (header, identifier, dimensions, _default) = &net.nodes;
                let header = header.as_ref().map(RefNode::NetPortHeaderOrInterfacePortHeader);
                (header, identifier, dimensions.iter().collect::<Vec<_>>())
            }
            sv::AnsiPortDeclaration::Variable(variable) => {
                let (header, identifier, dimensions, _default) = &variable.nodes;
                let header = header.as_ref().map(RefNode::VariablePortHeader);
                let dimensions = dimensions
                    .iter()
                    .filter_map(|d| match d {
                        sv::VariableDimension::UnpackedDimension(u) => Some(&**u),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                (header, identifier, dimensions)
            }
            sv::AnsiPortDeclaration::Paren(paren) => {
                self.unsupported_item(tree, RefNode::AnsiPortDeclarationParen(paren));
                return None;
            }
        };

        let name = nav::identifier_str(tree, RefNode::PortIdentifier(identifier))?;
        let span = self.span(tree, RefNode::PortIdentifier(identifier));
        // The span names the port; the extent is its whole declaration, since
        // changing a port's width or direction rewrites the type in front of
        // the name rather than the name.

        // `bus_if.slave s`, `interface g`, `interface.master h`: a port that
        // is a whole interface rather than a wire. It has no direction, type
        // or width of its own; elaboration unfolds it into the interface's
        // signals, one port each.
        if let Some(RefNode::InterfacePortHeader(header)) =
            header_node.clone().and_then(|h| unwrap_node!(h, InterfacePortHeader))
        {
            let iface = self.interface_port_header(tree, header);
            return Some(UPort {
                name,
                written: None,
                dir: None,
                net_type: None,
                type_name: None,
                packed: None,
                unpacked: None,
                iface: Some(iface),
                span,
            });
        }

        let dir = header_node
            .clone()
            .and_then(|h| port_direction(tree, h))
            .or_else(|| inherited.and_then(|p| p.dir))
            .or(Some(PortDir::Inout));
        // The type comes from this port's own header, or — when it has none at
        // all — from the port it was declared alongside.
        let (net_type, packed) = match &header_node {
            Some(header) => {
                (net_type_of(tree, header.clone()), self.packed_range(tree, header.clone()))
            }
            None => match inherited {
                Some(previous) => (previous.net_type, previous.packed.clone()),
                None => (None, None),
            },
        };
        let unpacked = dimensions
            .first()
            .and_then(|d| self.unpacked_range(tree, RefNode::UnpackedDimension(d)));

        let type_name = match &header_node {
            Some(header) => user_type_name(tree, header.clone()),
            None => inherited.and_then(|previous| previous.type_name.clone()),
        };
        Some(UPort {
            name,
            written: None,
            dir,
            net_type,
            type_name,
            packed,
            unpacked,
            iface: None,
            span,
        })
    }

    /// The `= expr` of a declaration that drives what it declares.
    ///
    /// Only for a variable or net name: the dynamic-array and class forms carry
    /// something that is not an expression at all.
    fn decl_initialiser(&mut self, tree: &SyntaxTree, event: &RefNode<'_>) -> Option<UExpr> {
        let expression = match event {
            RefNode::NetDeclAssignment(assignment) => {
                assignment.nodes.2.as_ref().map(|(_eq, expression)| expression)
            }
            // Only the plain variable form: the dynamic-array and class forms
            // carry a `new`, which is not an expression at all.
            RefNode::VariableDeclAssignment(sv::VariableDeclAssignment::Variable(variable)) => {
                variable.nodes.2.as_ref().map(|(_eq, expression)| expression)
            }
            _ => None,
        }?;
        Some(self.expr(tree).expression(expression))
    }

    /// The `[msb:lsb]` of a vector declaration, if it has one.
    fn packed_range(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> Option<URange> {
        let range = unwrap_node!(node, PackedDimensionRange)?;
        let RefNode::PackedDimensionRange(range) = range else { return None };
        self.constant_range(tree, &range.nodes.0.nodes.1)
    }

    /// The array dimension that makes a declaration a memory.
    ///
    /// `[0:N-1]` is taken as written; the `[N]` shorthand is desugared to
    /// `[N-1:0]`, which is what it means, not a guess.
    fn unpacked_range(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> Option<URange> {
        let dimension = unwrap_node!(node, UnpackedDimension)?;
        let RefNode::UnpackedDimension(dimension) = dimension else { return None };
        match dimension {
            sv::UnpackedDimension::Range(range) => {
                self.constant_range(tree, &range.nodes.0.nodes.1)
            }
            sv::UnpackedDimension::Expression(expression) => {
                let (_open, inner, _close) = &expression.nodes.0.nodes;
                let size = self.expr(tree).constant_expression(inner);
                let span = size.span;
                let one = UExpr::int(1, span);
                let msb =
                    UExpr::new(UExprKind::Binary { op: BinOp::Sub, lhs: size, rhs: one }, span);
                Some(URange { msb, lsb: UExpr::int(0, span) })
            }
        }
    }

    fn constant_range(&mut self, tree: &SyntaxTree, range: &sv::ConstantRange) -> Option<URange> {
        let (msb, _colon, lsb) = &range.nodes;
        let msb = self.expr(tree).constant_expression(msb);
        let lsb = self.expr(tree).constant_expression(lsb);
        Some(URange { msb, lsb })
    }

    // -------------------------------------------------------------- items ---

    fn non_port_module_item(
        &mut self,
        tree: &SyntaxTree,
        item: &sv::NonPortModuleItem,
    ) -> Vec<UItem> {
        match item {
            // `generate ... endgenerate` is only a wrapper — IEEE 1800 makes the
            // keywords optional — so it contributes no structure of its own.
            sv::NonPortModuleItem::GenerateRegion(region) => {
                let (_keyword, items, _end) = &region.nodes;
                self.generate_items(tree, items)
            }
            sv::NonPortModuleItem::ModuleOrGenerateItem(inner) => {
                self.module_or_generate_item(tree, RefNode::ModuleOrGenerateItem(inner))
            }
            other => vec![self.unsupported_item(tree, RefNode::NonPortModuleItem(other))],
        }
    }

    fn generate_items(&mut self, tree: &SyntaxTree, items: &[sv::GenerateItem]) -> Vec<UItem> {
        let mut out = Vec::new();
        for item in items {
            match item {
                sv::GenerateItem::ModuleOrGenerateItem(inner) => out.extend(
                    self.module_or_generate_item(tree, RefNode::ModuleOrGenerateItem(inner)),
                ),
                other => out.push(self.unsupported_item(tree, RefNode::GenerateItem(other))),
            }
        }
        out
    }

    /// Lowers the body of a generate construct.
    ///
    /// A labelled `begin : g_tap` becomes one [`UItem::GenerateBlock`] rather
    /// than loose items, because that label is what names the unrolled copies:
    /// `g_tap[0].u_x`. An unlabelled block contributes only its contents.
    fn generate_block(&mut self, tree: &SyntaxTree, block: &sv::GenerateBlock) -> Vec<UItem> {
        match block {
            sv::GenerateBlock::GenerateItem(item) => {
                self.generate_items(tree, std::slice::from_ref(item))
            }
            sv::GenerateBlock::Multiple(multiple) => {
                let (leading, _begin, trailing, items, _end, _tail) = &multiple.nodes;
                // The label may be written before `begin` or after it.
                let label = leading
                    .as_ref()
                    .map(|(id, _colon)| RefNode::GenerateBlockIdentifier(id))
                    .or_else(|| {
                        trailing.as_ref().map(|(_colon, id)| RefNode::GenerateBlockIdentifier(id))
                    })
                    .and_then(|node| nav::identifier_str(tree, node));
                let span = self.span(tree, RefNode::GenerateBlockMultiple(multiple));
                let items = self.generate_items(tree, items);
                match label {
                    Some(label) => vec![UItem::GenerateBlock { label: Some(label), items, span }],
                    None => items,
                }
            }
        }
    }

    fn loop_generate(&mut self, tree: &SyntaxTree, construct: &sv::LoopGenerateConstruct) -> UItem {
        let (_keyword, paren, block) = &construct.nodes;
        let (_open, (initialisation, _semi, condition, _semi2, iteration), _close) = &paren.nodes;
        let (_genvar_keyword, genvar_identifier, _eq, init_expression) = &initialisation.nodes;

        let genvar = nav::identifier_str(tree, RefNode::GenvarIdentifier(genvar_identifier))
            .unwrap_or_else(|| "<genvar>".to_string());
        let init = self.expr(tree).constant_expression(init_expression);
        let cond = self.expr(tree).constant_expression(&condition.nodes.0);
        let step = self.genvar_step(tree, iteration, &genvar);
        let span = self.span(tree, RefNode::LoopGenerateConstruct(construct));
        let body = self.generate_block(tree, block);

        UItem::GenerateFor { genvar, init, cond, step, body, span }
    }

    /// The value the genvar takes on the next iteration.
    ///
    /// `i = i + 1` hands over its right-hand side directly; `i++` and `++i` are
    /// desugared to `i + 1`, which is what they mean.
    fn genvar_step(
        &mut self,
        tree: &SyntaxTree,
        iteration: &sv::GenvarIteration,
        genvar: &str,
    ) -> UExpr {
        match iteration {
            sv::GenvarIteration::Assignment(assignment) => {
                let (_identifier, operator, expression) = &assignment.nodes;
                if self.keyword_of(tree, RefNode::AssignmentOperator(operator)) == "=" {
                    self.expr(tree).constant_expression(&expression.nodes.0)
                } else {
                    // `i += 1` would need the operator folded into the step.
                    self.expr(tree)
                        .unsupported_public(RefNode::GenvarIterationAssignment(assignment))
                }
            }
            sv::GenvarIteration::Prefix(prefix) => {
                let (operator, _identifier) = &prefix.nodes;
                self.inc_dec_step(
                    tree,
                    RefNode::IncOrDecOperator(operator),
                    genvar,
                    RefNode::GenvarIterationPrefix(prefix),
                )
            }
            sv::GenvarIteration::Suffix(suffix) => {
                let (_identifier, operator) = &suffix.nodes;
                self.inc_dec_step(
                    tree,
                    RefNode::IncOrDecOperator(operator),
                    genvar,
                    RefNode::GenvarIterationSuffix(suffix),
                )
            }
        }
    }

    fn inc_dec_step(
        &mut self,
        tree: &SyntaxTree,
        operator: RefNode<'_>,
        genvar: &str,
        whole: RefNode<'_>,
    ) -> UExpr {
        let text = self.keyword_of(tree, operator);
        let span = self.span(tree, whole.clone());
        let op = match text.as_str() {
            "++" => BinOp::Add,
            "--" => BinOp::Sub,
            _ => return self.expr(tree).unsupported_public(whole),
        };
        UExpr::new(
            UExprKind::Binary { op, lhs: UExpr::ident(genvar, span), rhs: UExpr::int(1, span) },
            span,
        )
    }

    fn if_generate(&mut self, tree: &SyntaxTree, construct: &sv::IfGenerateConstruct) -> UItem {
        let (_keyword, paren, then_block, else_block) = &construct.nodes;
        let cond = self.expr(tree).constant_expression(&paren.nodes.1);
        let span = self.span(tree, RefNode::IfGenerateConstruct(construct));
        let then_items = self.generate_block(tree, then_block);
        let else_items = match else_block {
            Some((_keyword, block)) => self.generate_block(tree, block),
            None => Vec::new(),
        };
        UItem::GenerateIf { cond, then_items, else_items, span }
    }

    fn module_item(&mut self, tree: &SyntaxTree, item: &sv::ModuleItem) -> Vec<UItem> {
        match item {
            sv::ModuleItem::PortDeclaration(declaration) => {
                self.port_declaration(tree, RefNode::PortDeclaration(&declaration.0))
            }
            sv::ModuleItem::NonPortModuleItem(inner) => self.non_port_module_item(tree, inner),
        }
    }

    /// Classifies a body item by looking for the construct it contains.
    ///
    /// Generate constructs are checked first: they nest instantiations and
    /// declarations, and a search that found those would report an unrolled
    /// design that was never elaborated.
    fn module_or_generate_item(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> Vec<UItem> {
        if let Some(found) = unwrap_node!(
            node.clone(),
            GenerateRegion,
            LoopGenerateConstruct,
            IfGenerateConstruct,
            CaseGenerateConstruct
        ) {
            return match found {
                RefNode::GenerateRegion(region) => self.generate_items(tree, &region.nodes.1),
                RefNode::LoopGenerateConstruct(construct) => {
                    vec![self.loop_generate(tree, construct)]
                }
                RefNode::IfGenerateConstruct(construct) => vec![self.if_generate(tree, construct)],
                // `case` generate is outside the subset (D2).
                other => vec![self.unsupported_item(tree, other)],
            };
        }

        // `defparam` is rejected by name rather than lumped in with the generic
        // skips: it is the one construct the subset refuses on purpose (D2),
        // because it makes a parameter's value depend on where you look rather
        // than on the instantiation.
        if let Some(override_node) = unwrap_node!(node.clone(), ParameterOverride) {
            let text = nav::subtree_text(tree, override_node.clone());
            let span = self.span(tree, override_node);
            self.diags.push(
                Diag::error(
                    DiagCode::DefparamUnsupported,
                    "`defparam` is not supported; move the value into the instantiation",
                )
                .at(span),
            );
            return vec![UItem::Defparam { text, span }];
        }

        if let Some(RefNode::ModuleInstantiation(instantiation)) =
            unwrap_node!(node.clone(), ModuleInstantiation)
        {
            return self.module_instantiation(tree, instantiation);
        }

        if let Some(RefNode::FunctionDeclaration(function)) =
            unwrap_node!(node.clone(), FunctionDeclaration)
        {
            return vec![self.function_declaration(tree, function)];
        }

        if let Some(RefNode::TaskDeclaration(task)) = unwrap_node!(node.clone(), TaskDeclaration) {
            return vec![self.task_declaration(tree, task)];
        }

        // Before declarations, and this order is load-bearing. An `always`
        // block may declare a variable inside it:
        //
        //     always_ff @(posedge clk) begin
        //         automatic logic tmp;
        //         ...
        //
        // and a search for a declaration finds that inner one, classifies the
        // whole block as a net declaration, and the process disappears — with
        // no diagnostic, because as far as the lowering knew it had understood
        // the item. Measured on real RTL: whole `always_ff` blocks vanished
        // this way, and the module came back with zero processes and nothing
        // reported.
        if let Some(RefNode::ContinuousAssign(assign)) =
            unwrap_node!(node.clone(), ContinuousAssign)
        {
            return self.continuous_assign(tree, RefNode::ContinuousAssign(assign));
        }

        if let Some(RefNode::AlwaysConstruct(construct)) =
            unwrap_node!(node.clone(), AlwaysConstruct)
        {
            return vec![self.always_construct(tree, construct)];
        }

        if let Some(RefNode::InitialConstruct(construct)) =
            unwrap_node!(node.clone(), InitialConstruct)
        {
            return vec![self.initial_construct(tree, construct)];
        }

        // An `import` is a data declaration as far as the grammar is concerned,
        // so it has to be met before the declaration search below, which
        // would find no name in it and quietly lower it to nothing.
        if let Some(RefNode::PackageImportDeclaration(import)) =
            unwrap_node!(node.clone(), PackageImportDeclaration)
        {
            self.package_import(tree, import);
            return Vec::new();
        }

        if let Some(RefNode::TypeDeclarationDataType(declaration)) =
            unwrap_node!(node.clone(), TypeDeclarationDataType)
            && let Some(item) = self.typedef(tree, declaration)
        {
            return vec![item];
        }

        if unwrap_node!(node.clone(), NetDeclaration, DataDeclaration).is_some() {
            return self.net_declaration(tree, node);
        }

        if let Some(RefNode::LocalParameterDeclaration(declaration)) =
            unwrap_node!(node.clone(), LocalParameterDeclaration)
        {
            let mut params = Vec::new();
            self.local_parameter_declaration(tree, declaration, &mut params);
            return params.into_iter().map(|param| UItem::Param { param }).collect();
        }

        if let Some(RefNode::ParameterDeclaration(declaration)) =
            unwrap_node!(node.clone(), ParameterDeclaration)
        {
            let mut params = Vec::new();
            self.parameter_declaration(tree, declaration, false, &mut params);
            return params.into_iter().map(|param| UItem::Param { param }).collect();
        }

        if unwrap_node!(node.clone(), PortDeclaration).is_some() {
            return self.port_declaration(tree, node);
        }

        // A `genvar` declaration says nothing the loop header does not repeat,
        // so it is handled by being dropped — not skipped, and so not reported.
        if unwrap_node!(node.clone(), GenvarDeclaration).is_some() {
            return Vec::new();
        }

        vec![self.unsupported_item(tree, node)]
    }

    // ---------------------------------------------------------- functions ---

    /// `function automatic logic [7:0] sat(input logic [21:0] a); ... endfunction`
    ///
    /// Captured whole. A function is not hardware on its own — it becomes logic
    /// only where it is called — so elaboration copies the body into each call
    /// site rather than this pass turning it into anything.
    fn function_declaration(
        &mut self,
        tree: &SyntaxTree,
        function: &sv::FunctionDeclaration,
    ) -> UItem {
        let whole = RefNode::FunctionDeclaration(function);
        let span = self.span(tree, whole.clone());
        let (_keyword, _lifetime, body) = &function.nodes;

        // The return type sits between the `function` keyword and the name, so
        // the first packed range under the declaration is the return width.
        let packed = match body {
            sv::FunctionBodyDeclaration::WithPort(with) => {
                self.packed_range(tree, RefNode::FunctionDataTypeOrImplicit(&with.nodes.0))
            }
            sv::FunctionBodyDeclaration::WithoutPort(without) => {
                self.packed_range(tree, RefNode::FunctionDataTypeOrImplicit(&without.nodes.0))
            }
        };

        let return_node = match body {
            sv::FunctionBodyDeclaration::WithPort(with) => {
                RefNode::FunctionDataTypeOrImplicit(&with.nodes.0)
            }
            sv::FunctionBodyDeclaration::WithoutPort(without) => {
                RefNode::FunctionDataTypeOrImplicit(&without.nodes.0)
            }
        };
        let net_type = net_type_of(tree, return_node.clone());
        let type_name = user_type_name(tree, return_node);

        let (name_node, statements) = match body {
            sv::FunctionBodyDeclaration::WithPort(with) => {
                (RefNode::FunctionIdentifier(&with.nodes.2), &with.nodes.6)
            }
            sv::FunctionBodyDeclaration::WithoutPort(without) => {
                (RefNode::FunctionIdentifier(&without.nodes.2), &without.nodes.5)
            }
        };
        let Some(name) = nav::identifier_str(tree, name_node) else {
            return self.unsupported_item(tree, whole);
        };

        // Arguments come either from the port list or, in the `without port`
        // form, from `input`-qualified declarations in the body.
        let mut args = Vec::new();
        if let sv::FunctionBodyDeclaration::WithPort(with) = body
            && let Some(list) = &with.nodes.3.nodes.1
        {
            self.tf_port_list(tree, list, &mut args);
        }
        for event in whole.clone().into_iter() {
            if let RefNode::TfPortDeclaration(declaration) = event {
                self.tf_port_declaration(tree, declaration, &mut args);
            }
        }

        // Locals are every other declaration in the body. Reusing the net path
        // means an array or a user-defined type inside a function is sized the
        // same way it would be at module level.
        let locals = self.tf_locals(tree, whole.clone());

        let outer = self.fn_name.replace(name.clone());
        let outer_flag = std::mem::replace(&mut self.fn_flag, early_return_flag(&name, statements));
        let flag = self.fn_flag.clone();
        let body = self.function_body(tree, statements, span);
        self.fn_name = outer;
        self.fn_flag = outer_flag;

        let mut locals = locals;
        if let Some(flag) = flag {
            // The flag is a net like any other local, so the inliner gives each
            // call site its own copy without knowing what it is for.
            locals.push(UNet {
                name: flag,
                written: None,
                net_type: UNetType::Logic,
                type_name: None,
                packed: None,
                unpacked: None,
                span,
            });
        }
        UItem::Function {
            func: UFunction {
                name,
                is_task: false,
                packed,
                net_type,
                type_name,
                args,
                locals,
                body,
                package: None,
                span,
            },
        }
    }

    /// `task automatic load_tx_byte(...); ... endtask`
    ///
    /// A task is a function with nothing coming back: it is called as a
    /// statement, and hands its results over through `output` arguments. The
    /// body is captured the same way, and inlined the same way.
    fn task_declaration(&mut self, tree: &SyntaxTree, task: &sv::TaskDeclaration) -> UItem {
        let whole = RefNode::TaskDeclaration(task);
        let span = self.span(tree, whole.clone());
        let (_keyword, _lifetime, body) = &task.nodes;

        let (name_node, statements) = match body {
            sv::TaskBodyDeclaration::WithPort(with) => {
                (RefNode::TaskIdentifier(&with.nodes.1), &with.nodes.5)
            }
            sv::TaskBodyDeclaration::WithoutPort(without) => {
                (RefNode::TaskIdentifier(&without.nodes.1), &without.nodes.4)
            }
        };
        let Some(name) = nav::identifier_str(tree, name_node) else {
            return self.unsupported_item(tree, whole);
        };

        let mut args = Vec::new();
        if let sv::TaskBodyDeclaration::WithPort(with) = body
            && let Some(list) = &with.nodes.2.nodes.1
        {
            self.tf_port_list(tree, list, &mut args);
        }
        for event in whole.clone().into_iter() {
            if let RefNode::TfPortDeclaration(declaration) = event {
                self.tf_port_declaration(tree, declaration, &mut args);
            }
        }

        let locals = self.tf_locals(tree, whole);

        let outer = self.fn_name.replace(name.clone());
        let stmts =
            statements.iter().map(|statement| self.statement_or_null(tree, statement)).collect();
        self.fn_name = outer;

        UItem::Function {
            func: UFunction {
                name,
                is_task: true,
                packed: None,
                net_type: None,
                type_name: None,
                args,
                locals,
                body: UStmt::new(UStmtKind::Block { stmts, decls: Vec::new() }, span),
                package: None,
                span,
            },
        }
    }

    /// The variables declared inside a function or task body.
    fn tf_locals(&mut self, tree: &SyntaxTree, whole: RefNode<'_>) -> Vec<UNet> {
        let mut locals = Vec::new();
        for event in whole.into_iter() {
            let RefNode::BlockItemDeclaration(declaration) = event else { continue };
            for item in self.net_declaration(tree, RefNode::BlockItemDeclaration(declaration)) {
                if let UItem::Net { net } = item {
                    locals.push(net);
                }
            }
        }
        locals
    }

    /// A function or task's argument list, in order.
    ///
    /// An item with no type of its own inherits the previous one's, which is
    /// what `input logic [7:0] b0, b1` means. Getting that wrong does not just
    /// mis-size `b1` — the grammar hands its *name* over in the empty type
    /// slot, so the argument disappears entirely and every call to the function
    /// looks like it passed one argument too many.
    fn tf_port_list(
        &mut self,
        tree: &SyntaxTree,
        list: &sv::TfPortList,
        out: &mut Vec<UFunctionArg>,
    ) {
        let mut inherited: Option<(Option<URange>, Option<UNetType>, Option<String>)> = None;
        // A direction carries forward the same way a type does.
        let mut dir = PortDir::Input;
        for item in list.nodes.0.contents() {
            let (_attributes, direction, _var, data_type, identifier) = &item.nodes;
            if let Some(direction) = direction
                && let Some(explicit) = tf_port_direction(tree, direction)
            {
                dir = explicit;
            }
            let (name, span, declared) = match identifier {
                Some((port_identifier, _dimensions, _default)) => {
                    let Some(name) =
                        nav::identifier_str(tree, RefNode::PortIdentifier(port_identifier))
                    else {
                        continue;
                    };
                    let span = self.span(tree, RefNode::PortIdentifier(port_identifier));
                    let packed = self.packed_range(tree, RefNode::TfPortItem(item));
                    let net_type = net_type_of(tree, RefNode::TfPortItem(item));
                    let type_name = user_type_name(tree, RefNode::TfPortItem(item));
                    let declared = (packed.is_some() || net_type.is_some() || type_name.is_some())
                        .then_some((packed, net_type, type_name));
                    (name, span, declared)
                }
                // No name slot: the parser could not tell `b1` from a type, and
                // put it where a type would go.
                None => {
                    let node = RefNode::DataTypeOrImplicit(data_type);
                    let Some(name) = nav::identifier_str(tree, node.clone()) else { continue };
                    (name, self.span(tree, node), None)
                }
            };

            let (packed, net_type, type_name) = match declared {
                Some(declared) => {
                    inherited = Some(declared.clone());
                    declared
                }
                None => inherited.clone().unwrap_or_default(),
            };
            out.push(UFunctionArg { name, dir, packed, net_type, type_name, span });
        }
    }

    /// `input logic [7:0] a, b;` inside a body-style function declaration.
    fn tf_port_declaration(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::TfPortDeclaration,
        out: &mut Vec<UFunctionArg>,
    ) {
        let packed = self.packed_range(tree, RefNode::TfPortDeclaration(declaration));
        let net_type = net_type_of(tree, RefNode::TfPortDeclaration(declaration));
        let type_name = user_type_name(tree, RefNode::TfPortDeclaration(declaration));
        let dir = tf_port_direction(tree, &declaration.nodes.1).unwrap_or(PortDir::Input);
        for (port_identifier, _dimensions, _default) in
            declaration.nodes.4.nodes.0.contents().iter()
        {
            let Some(name) = nav::identifier_str(tree, RefNode::PortIdentifier(port_identifier))
            else {
                continue;
            };
            let span = self.span(tree, RefNode::PortIdentifier(port_identifier));
            out.push(UFunctionArg {
                name,
                dir,
                packed: packed.clone(),
                net_type,
                type_name: type_name.clone(),
                span,
            });
        }
    }

    fn function_body(
        &mut self,
        tree: &SyntaxTree,
        statements: &[sv::FunctionStatementOrNull],
        span: Span,
    ) -> UStmt {
        let returns: Vec<bool> = statements
            .iter()
            .map(|s| unwrap_node!(RefNode::FunctionStatementOrNull(s), JumpStatement).is_some())
            .collect();
        let lowered =
            statements.iter().map(|statement| self.function_statement(tree, statement)).collect();
        let mut stmts = self.guard_after_return(lowered, &returns, span);

        // The flag starts clear, so the first statement runs unconditionally.
        if let Some(flag) = self.fn_flag.clone() {
            stmts.insert(
                0,
                UStmt::new(
                    UStmtKind::Assign {
                        lhs: UExpr::ident(&flag, span),
                        rhs: UExpr::int(0, span),
                        blocking: true,
                    },
                    span,
                ),
            );
        }
        UStmt::new(UStmtKind::Block { stmts, decls: Vec::new() }, span)
    }

    fn port_declaration(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> Vec<UItem> {
        let Some(RefNode::PortDeclaration(declaration)) =
            unwrap_node!(node.clone(), PortDeclaration)
        else {
            return vec![self.unsupported_item(tree, node)];
        };

        let (dir, inner) = match declaration {
            sv::PortDeclaration::Input(d) => (PortDir::Input, RefNode::PortDeclarationInput(d)),
            sv::PortDeclaration::Output(d) => (PortDir::Output, RefNode::PortDeclarationOutput(d)),
            sv::PortDeclaration::Inout(d) => (PortDir::Inout, RefNode::PortDeclarationInout(d)),
            other => return vec![self.unsupported_item(tree, RefNode::PortDeclaration(other))],
        };

        let packed = self.packed_range(tree, inner.clone());
        let net_type = net_type_of(tree, inner.clone());
        let mut out = Vec::new();
        for name_node in identifiers_of(inner.clone()) {
            let Some(name) = tree.get_str(&name_node).map(str::to_owned) else { continue };
            let span = self.map.resolve(tree, &name_node);
            out.push(UItem::PortDecl {
                name,
                dir,
                net_type,
                packed: packed.clone(),
                unpacked: None,
                span,
            });
        }
        if out.is_empty() {
            return vec![self.unsupported_item(tree, inner)];
        }
        out
    }

    /// `typedef enum logic [1:0] { IDLE, RUN } state_e;`
    ///
    /// Returns `None` for a typedef of anything else — a struct, a union, an
    /// alias — so the caller can report it as the gap it is.
    fn typedef(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::TypeDeclarationDataType,
    ) -> Option<UItem> {
        let (_keyword, data_type, identifier, _dimensions, _semi) = &declaration.nodes;
        let name = nav::identifier_str(tree, RefNode::TypeIdentifier(identifier))?;
        let span = self.span(tree, RefNode::TypeDeclarationDataType(declaration));

        let enumeration = match data_type {
            sv::DataType::Enum(enumeration) => enumeration,
            sv::DataType::StructUnion(structure) => {
                return Some(self.typedef_struct(tree, declaration, structure, name, span));
            }
            _ => return None,
        };
        let (_enum_keyword, base, members, _packed) = &enumeration.nodes;

        // `enum logic [1:0]` gives the width; a bare `enum` is `int`, which the
        // range being absent stands for.
        let packed =
            base.as_ref().and_then(|base| self.packed_range(tree, RefNode::EnumBaseType(base)));

        let members = members
            .nodes
            .1
            .contents()
            .into_iter()
            .filter_map(|member| {
                let (identifier, range, value) = &member.nodes;
                if range.is_some() {
                    // `IDLE[3]` declares four members at once. Rare, and
                    // guessing at it would invent names that are not there.
                    self.unsupported_item(tree, RefNode::EnumNameDeclaration(member));
                    return None;
                }
                let name = nav::identifier_str(tree, RefNode::EnumIdentifier(identifier))?;
                let span = self.span(tree, RefNode::EnumNameDeclaration(member));
                let value = value
                    .as_ref()
                    .map(|(_eq, expression)| self.expr(tree).constant_expression(expression));
                Some(rtlscope_ir::UEnumMember { name, written: None, value, span })
            })
            .collect();

        Some(UItem::TypedefEnum { name, packed, members, span })
    }

    /// `typedef struct packed { logic [7:0] value; logic valid; } beat_t;`
    ///
    /// Each member is lowered as the net declaration it is shaped like, so a
    /// member of a typedef'd type or with a range of its own is carried the
    /// same way a net is. A union, or a struct that is not packed, has no
    /// single width to give a variable, and is reported rather than guessed.
    fn typedef_struct(
        &mut self,
        tree: &SyntaxTree,
        declaration: &sv::TypeDeclarationDataType,
        structure: &sv::DataTypeStructUnion,
        name: String,
        span: Span,
    ) -> UItem {
        let (kind, packed, members, _dimensions) = &structure.nodes;
        let is_packed_struct = matches!(kind, sv::StructUnion::Struct(_)) && packed.is_some();
        if !is_packed_struct {
            return self.unsupported_item(tree, RefNode::TypeDeclarationDataType(declaration));
        }
        let (first, rest) = &members.nodes.1;
        let members = std::iter::once(first)
            .chain(rest)
            .flat_map(|member| self.net_declaration(tree, RefNode::StructUnionMember(member)))
            .filter_map(|item| match item {
                UItem::Net { net } => Some(net),
                _ => None,
            })
            .collect();
        UItem::TypedefStruct { name, members, span }
    }

    pub(crate) fn net_declaration(&mut self, tree: &SyntaxTree, node: RefNode<'_>) -> Vec<UItem> {
        // A user-defined type is carried by name rather than reported here.
        // Elaboration holds the typedefs and can size `state_e state;` properly;
        // only a name it still cannot resolve becomes a diagnostic, and that is
        // its call to make, not this one's.
        let type_name = user_type_name(tree, node.clone());
        let net_type = net_type_of(tree, node.clone()).unwrap_or(UNetType::Logic);
        let packed = self.packed_range(tree, node.clone());

        let mut out = Vec::new();
        for event in node.clone().into_iter() {
            let (identifier_node, dimension_search) = match &event {
                RefNode::VariableDeclAssignment(x) => {
                    (RefNode::VariableDeclAssignment(x), RefNode::VariableDeclAssignment(x))
                }
                RefNode::NetDeclAssignment(x) => {
                    (RefNode::NetDeclAssignment(x), RefNode::NetDeclAssignment(x))
                }
                _ => continue,
            };
            let Some(name) = nav::identifier_str(tree, identifier_node.clone()) else { continue };
            let span = self.span(tree, identifier_node);
            let unpacked = self.unpacked_range(tree, dimension_search);
            out.push(UItem::Net {
                net: UNet {
                    name: name.clone(),
                    written: None,
                    net_type,
                    type_name: type_name.clone(),
                    packed: packed.clone(),
                    unpacked,
                    span,
                },
            });

            // `wire [40:0] entry = rom[index];` declares a net *and* drives
            // it. Dropping the initialiser loses the whole assignment, so the
            // net looks undriven and everything it reads looks unread.
            //
            // A *variable* with an initialiser means something else:
            // `logic [7:0] count = 8'h00;` is the value it powers up holding,
            // not logic that drives it forever. Modelling it as a continuous
            // assignment would give it a second driver fighting the `always_ff`
            // that owns it.
            if let Some(value) = self.decl_initialiser(tree, &event) {
                let lhs = UExpr::ident(&name, span);
                out.push(match event {
                    RefNode::NetDeclAssignment(_) => UItem::Assign { lhs, rhs: value, span },
                    _ => UItem::Proc {
                        kind: UProcKind::Initial,
                        body: UStmt::new(
                            UStmtKind::Assign { lhs, rhs: value, blocking: true },
                            span,
                        ),
                        span,
                    },
                });
            }
        }

        if out.is_empty() {
            return vec![self.unsupported_item(tree, node)];
        }
        out
    }

    fn module_instantiation(
        &mut self,
        tree: &SyntaxTree,
        instantiation: &sv::ModuleInstantiation,
    ) -> Vec<UItem> {
        let (module_identifier, parameters, instances, _semi) = &instantiation.nodes;
        let Some(module_name) =
            nav::identifier_str(tree, RefNode::ModuleIdentifier(module_identifier))
        else {
            return vec![self.unsupported_item(tree, RefNode::ModuleInstantiation(instantiation))];
        };

        let param_overrides = parameters
            .as_ref()
            .map(|p| self.parameter_value_assignment(tree, p))
            .unwrap_or(UParamOverrides::Empty);

        let mut out = Vec::new();
        for instance in instances.contents() {
            let (name_of_instance, connections) = &instance.nodes;
            let Some(name) = nav::identifier_str(tree, RefNode::NameOfInstance(name_of_instance))
            else {
                continue;
            };
            let span = self.span(tree, RefNode::HierarchicalInstance(instance));
            let conns = match &connections.nodes.1 {
                Some(list) => self.port_connections(tree, list),
                None => UConns::Empty,
            };
            out.push(UItem::Inst {
                inst: UInstance {
                    module_name: module_name.clone(),
                    name,
                    written: None,
                    param_overrides: param_overrides.clone(),
                    conns,
                    span,
                },
            });
        }
        out
    }

    fn parameter_value_assignment(
        &mut self,
        tree: &SyntaxTree,
        assignment: &sv::ParameterValueAssignment,
    ) -> UParamOverrides {
        let (_hash, paren) = &assignment.nodes;
        let Some(list) = &paren.nodes.1 else { return UParamOverrides::Empty };
        match list {
            sv::ListOfParameterAssignments::Ordered(ordered) => {
                let values = ordered
                    .nodes
                    .0
                    .contents()
                    .into_iter()
                    .map(|entry| self.param_expression(tree, &entry.nodes.0))
                    .collect();
                UParamOverrides::Positional { values }
            }
            sv::ListOfParameterAssignments::Named(named) => {
                let mut values = Vec::new();
                for entry in named.nodes.0.contents() {
                    let (_dot, identifier, paren) = &entry.nodes;
                    let Some(name) =
                        nav::identifier_str(tree, RefNode::ParameterIdentifier(identifier))
                    else {
                        continue;
                    };
                    let Some(expression) = &paren.nodes.1 else { continue };
                    values.push((name, self.param_expression(tree, expression)));
                }
                UParamOverrides::Named { values }
            }
        }
    }

    fn param_expression(&mut self, tree: &SyntaxTree, expression: &sv::ParamExpression) -> UExpr {
        match expression {
            sv::ParamExpression::MintypmaxExpression(m) => match &**m {
                sv::MintypmaxExpression::Expression(e) => self.expr(tree).expression(e),
                sv::MintypmaxExpression::Ternary(_) => {
                    self.expr(tree).unsupported_public(RefNode::MintypmaxExpression(m))
                }
            },
            other => self.expr(tree).unsupported_public(RefNode::ParamExpression(other)),
        }
    }

    fn port_connections(&mut self, tree: &SyntaxTree, list: &sv::ListOfPortConnections) -> UConns {
        match list {
            sv::ListOfPortConnections::Ordered(ordered) => {
                let mut values = Vec::new();
                for entry in ordered.nodes.0.contents() {
                    let Some(node) = entry.nodes.1.as_ref() else {
                        values.push(None);
                        continue;
                    };
                    let expr = self.expr(tree).expression(node);
                    // A positional connection is written as a bare expression,
                    // so the expression is the text an edit replaces. Recorded
                    // here for the same reason `.port(net)` is: without it a
                    // wire connected this way could be read but not rewired.
                    values.push(Some(expr));
                }
                UConns::Positional { values }
            }
            sv::ListOfPortConnections::Named(named) => {
                let mut conns = Vec::new();
                let mut wildcard = false;
                for entry in named.nodes.0.contents() {
                    match entry {
                        sv::NamedPortConnection::Asterisk(_) => wildcard = true,
                        sv::NamedPortConnection::Identifier(identifier) => {
                            let (_attrs, _dot, port_identifier, paren) = &identifier.nodes;
                            let Some(port) =
                                nav::identifier_str(tree, RefNode::PortIdentifier(port_identifier))
                            else {
                                continue;
                            };
                            let span =
                                self.span(tree, RefNode::NamedPortConnectionIdentifier(identifier));
                            // `.port` with no parentheses is shorthand for
                            // `.port(port)`; normalise it here so elaboration
                            // only ever sees one form.
                            let expr = match paren {
                                // `.port` shorthand writes no expression, so
                                // there is no text of its own for one; the
                                // span it borrows is the connection's.
                                None => Some(UExpr::ident(port.clone(), span)),
                                Some(paren) => match paren.nodes.1.as_ref() {
                                    None => None,
                                    Some(node) => {
                                        let expr = self.expr(tree).expression(node);
                                        // The value inside the parentheses,
                                        // recorded separately from the whole
                                        // connection: rewiring replaces this
                                        // and must leave the author's spacing
                                        // around it alone.
                                        Some(expr)
                                    }
                                },
                            };
                            conns.push(UConn { port, expr, span });
                        }
                    }
                }
                if wildcard { UConns::Wildcard { conns } } else { UConns::Named { conns } }
            }
        }
    }
}

// ------------------------------------------------------- free-standing ---

fn port_direction(tree: &SyntaxTree, node: RefNode<'_>) -> Option<PortDir> {
    let direction = unwrap_node!(node, PortDirection)?;
    let text = nav::first_token(direction).and_then(|locate| tree.get_str(&locate))?;
    match text {
        "input" => Some(PortDir::Input),
        "output" => Some(PortDir::Output),
        "inout" => Some(PortDir::Inout),
        _ => None,
    }
}

/// The name of a user-defined type used in a declaration, if there is one.
///
/// Distinguishes `state_e state;` (a named type RTLScope cannot size) from
/// `input clk;` (no type written at all, which legally means a one-bit wire).
fn user_type_name(tree: &SyntaxTree, node: RefNode<'_>) -> Option<String> {
    // `defs::mode_t x;` — the package sits beside the name in the type node,
    // and is kept in front of it so elaboration looks the type up by the same
    // spelling a package's typedefs are bound under. The grammar gives a
    // named type in two shapes, and a bare name is a "class type" to it as
    // often as not, so both are read.
    match unwrap_node!(node.clone(), DataTypeType, ClassType) {
        Some(RefNode::DataTypeType(data_type)) => {
            let (scope, identifier, _dimensions) = &data_type.nodes;
            let name = nav::identifier_str(tree, RefNode::TypeIdentifier(identifier))?;
            let package = scope
                .as_ref()
                .and_then(|scope| nav::package_of(tree, RefNode::PackageScopeOrClassScope(scope)));
            return Some(nav::qualified(package.as_deref(), &name));
        }
        Some(RefNode::ClassType(class_type)) => {
            let (identifier, _params, nested) = &class_type.nodes;
            let (scope, name) = &identifier.nodes;
            // `p::c::x` names a class inside a package, which is not a type
            // this sizes; only the plain `p::t` and `t` are read.
            if !nested.is_empty() {
                return None;
            }
            let name = nav::identifier_str(tree, RefNode::ClassIdentifier(name))?;
            let package = scope
                .as_ref()
                .and_then(|scope| nav::package_of(tree, RefNode::PackageScope(scope)));
            return Some(nav::qualified(package.as_deref(), &name));
        }
        _ => {}
    }
    let type_node = unwrap_node!(node, NetTypeIdentifier, TypeIdentifier)?;
    nav::identifier_str(tree, type_node)
}

fn net_type_of(tree: &SyntaxTree, node: RefNode<'_>) -> Option<UNetType> {
    let type_node = unwrap_node!(node, IntegerVectorType, IntegerAtomType, NetType)?;
    let text = nav::first_token(type_node).and_then(|locate| tree.get_str(&locate))?;
    match text {
        "logic" => Some(UNetType::Logic),
        "bit" => Some(UNetType::Bit),
        "reg" => Some(UNetType::Reg),
        "wire" => Some(UNetType::Wire),
        "int" | "integer" => Some(UNetType::Integer),
        _ => None,
    }
}

/// Every identifier token declared by a port declaration's name list.
fn identifiers_of(node: RefNode<'_>) -> Vec<sv::Locate> {
    let mut out = Vec::new();
    for child in node.into_iter() {
        match child {
            RefNode::PortIdentifier(x) => {
                if let Some(locate) = nav::identifier(RefNode::PortIdentifier(x)) {
                    out.push(locate);
                }
            }
            RefNode::VariableIdentifier(x) => {
                if let Some(locate) = nav::identifier(RefNode::VariableIdentifier(x)) {
                    out.push(locate);
                }
            }
            _ => {}
        }
    }
    out
}

/// The direction an argument of a task or function was declared with.
fn tf_port_direction(tree: &SyntaxTree, direction: &sv::TfPortDirection) -> Option<PortDir> {
    let sv::TfPortDirection::PortDirection(direction) = direction else { return None };
    port_direction(tree, RefNode::PortDirection(direction))
}

/// The name of the "already returned" flag a function needs, or `None`.
///
/// Only a `return` with something after it needs one: the common tail `return`
/// is an assignment and nothing more.
fn early_return_flag(name: &str, statements: &[sv::FunctionStatementOrNull]) -> Option<String> {
    let leaves_early = statements.iter().enumerate().any(|(i, s)| {
        let has_return = unwrap_node!(RefNode::FunctionStatementOrNull(s), JumpStatement).is_some();
        has_return && i + 1 < statements.len()
    });
    // `$` is legal in an identifier after the first character and no one writes
    // it, so the flag cannot collide with a variable the function declared.
    let _ = name;
    leaves_early.then(|| "returned$".to_string())
}

#[cfg(test)]
mod written_name_tests {
    use super::{decode_generic, differing};

    /// Veryl names one module per way a generic is used; the written name is
    /// the generic's, so its arguments are put back from the read name's tail.
    #[test]
    fn a_generic_gets_its_arguments_back() {
        assert_eq!(
            decode_generic("veryl_testcase___Module55A__Module55B", "Module55A".into()),
            "Module55A::<Module55B>"
        );
        assert_eq!(
            decode_generic("prj___Module55I____Package55__8__16", "Module55I".into()),
            "Module55I::<Package55, 8, 16>",
            "nested arguments keep their names, not their brackets"
        );
        assert_eq!(decode_generic("lights_Control", "Control".into()), "Control", "not a generic");
        assert_eq!(decode_generic("prj___X__", "X".into()), "X", "a tail with nothing in it");
    }

    #[test]
    fn a_name_that_did_not_change_is_not_repeated() {
        assert_eq!(differing(Some("i_rst".into()), "i_rst_n"), Some("i_rst".into()));
        assert_eq!(differing(Some("i_start".into()), "i_start"), None);
        assert_eq!(differing(None, "x"), None);
    }
}
