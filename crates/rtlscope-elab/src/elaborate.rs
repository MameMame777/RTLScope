//! The elaboration pass.
//!
//! Turns the unresolved IR into a design where every width is a number, every
//! `generate` block has been unrolled, and every connection points at a net.
//!
//! The shape of the pass follows from one rule: **a module with different
//! parameters is a different module.** `params_sub #(.W(16))` and `params_sub`
//! cannot share a `Module`, because their ports are different widths. So
//! elaboration walks the instance tree from the top, and each `(name, bindings)`
//! pair it reaches becomes its own entry in the arena, memoised so a module
//! instantiated fifty times with the same parameters is built once.
//!
//! Process bodies are not lowered here yet — that is the next step. Everything
//! structural (hierarchy, ports, nets, connections) is complete, which is what
//! the block diagram is drawn from.

use std::collections::{HashMap, HashSet};

use rtlscope_ir::{
    Arena, Bundle, Conn, ConstBits, Design, Diag, DiagCode, Diagnostics, EnumMember, EnumType,
    IfaceInstance, Instance, Module, ModuleId, Net, NetId, NetKind, NetRef, Param, Port, PortDir,
    PortId, Process, Skipped, Span, UConn, UConns, UDesign, UExpr, UExprKind, UFunction,
    UIfacePort, UImport, UInstance, UInterface, UItem, UModule, UNetType, UParam, UParamOverrides,
    UPort, URange,
};

use crate::proc::{NameResolver, ProcBuilder};
use crate::value::{EvalError, Scope, eval};

/// How deep the instance tree may go before we call it a cycle.
const MAX_DEPTH: usize = 64;

/// How many iterations one `generate for` may run.
const MAX_GENERATE_ITERATIONS: i64 = 10_000;

/// Identifies one specialisation of a module: its name plus the parameter
/// bindings it was elaborated with, sorted so the key is canonical.
type SpecKey = (String, Vec<(String, i64)>, Vec<IfaceBinding>);

/// What an interface port of a module was connected to: the interface, the
/// parameters its instance was built with, and the modport when the port
/// itself named none. Part of the key a module is elaborated under, because
/// `interface c` is a different set of ports for every interface it is given.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct IfaceBinding {
    port: String,
    interface: String,
    params: Vec<(String, i64)>,
    modport: Option<String>,
}

/// An interface as it stands inside a module — an instance of one, or one of
/// the module's own interface ports — with the nets its signals became.
#[derive(Clone, Debug)]
struct IfaceInst {
    interface: String,
    params: Vec<(String, i64)>,
    modport: Option<String>,
    /// Signal name, and the net it is here.
    members: Vec<(String, NetId)>,
}

/// One signal of an interface, sized: what an instance declares and what a
/// port of it unfolds into.
struct Signal {
    name: String,
    written: Option<String>,
    width: u32,
    kind: NetKind,
    type_name: Option<String>,
    span: Span,
}

/// The width each user-defined type gives a declaration, within one module.
type TypeWidths = HashMap<String, u32>;

pub fn elaborate(uir: &UDesign, requested_top: Option<&str>) -> (Option<Design>, Diagnostics) {
    let mut elaborator = Elaborator::new(uir);
    elaborator.evaluate_packages();

    let Some(top_name) = elaborator.find_top(requested_top) else {
        return (None, elaborator.diags);
    };
    let top_span = uir.module_by_name(&top_name).map_or(Span::UNKNOWN, |m| m.span);
    let Some(top) = elaborator.elaborate_module(&top_name, &[], &[], top_span) else {
        return (None, elaborator.diags);
    };

    let mut modules = elaborator.modules;
    name_specialisations(&mut modules);
    let design =
        Design { modules, top, files: uir.files.clone(), generated: uir.generated.clone() };

    // The invariants are checked here rather than left to the caller: anything
    // this finds is an RTLScope bug, and a design that silently violates one
    // draws a wrong diagram instead of failing.
    let mut diags = elaborator.diags;
    diags.extend(crate::validate::validate(&design));
    (Some(design), diags)
}

struct Elaborator<'a> {
    uir: &'a UDesign,
    by_name: HashMap<&'a str, &'a UModule>,
    modules: Arena<Module>,
    cache: HashMap<SpecKey, ModuleId>,
    in_progress: HashSet<SpecKey>,
    diags: Diagnostics,
    depth: usize,
    /// Every package, evaluated once, in the order it was read.
    packages: Vec<(String, PackageInfo)>,
    interfaces: HashMap<&'a str, &'a UInterface>,
}

/// A package, evaluated: the names a module gets from it.
#[derive(Default, Clone)]
struct PackageInfo {
    /// Its constants — parameters and enum members — with their widths.
    scope: Scope,
    types: TypeWidths,
    enums: Vec<EnumType>,
    functions: HashMap<String, UFunction>,
}

impl<'a> Elaborator<'a> {
    fn new(uir: &'a UDesign) -> Self {
        let mut by_name = HashMap::new();
        for module in &uir.modules {
            by_name.entry(module.name.as_str()).or_insert(module);
        }
        Self {
            uir,
            by_name,
            modules: Arena::new(),
            cache: HashMap::new(),
            in_progress: HashSet::new(),
            diags: Diagnostics::new(),
            depth: 0,
            packages: Vec::new(),
            interfaces: {
                let mut interfaces = HashMap::new();
                for interface in &uir.interfaces {
                    interfaces.entry(interface.name.as_str()).or_insert(interface);
                }
                interfaces
            },
        }
    }

    // ------------------------------------------------------------ packages ---

    /// Evaluates every package once, in the order they were read.
    ///
    /// A package is a set of names, not hardware, so there is nothing to
    /// build: its parameters are folded, its typedefs sized, its enum members
    /// numbered and its functions kept for inlining, exactly as a module's own
    /// are — but once, here, rather than in every module that uses them. The
    /// items go in source order because each may use the ones above it, and a
    /// package may import another, which has to have been read before it.
    fn evaluate_packages(&mut self) {
        let uir = self.uir;
        for package in &uir.packages {
            let mut info = PackageInfo::default();
            self.seed(
                &package.imports,
                &mut info.scope,
                &mut info.types,
                &mut info.enums,
                &mut info.functions,
            );
            self.report_missing_imports(&package.imports);
            for item in &package.items {
                match item {
                    UItem::Param { param } => {
                        let value = param
                            .default
                            .as_ref()
                            .and_then(|default| {
                                self.try_eval_param(&param.name, default, &info.scope)
                            })
                            .unwrap_or(0);
                        let width = param
                            .packed
                            .as_ref()
                            .map(|range| self.width_of(Some(range), &info.scope, param.span));
                        match width {
                            Some(width) => info.scope.bind_sized(&param.name, value, width),
                            None => info.scope.bind(&param.name, value),
                        }
                    }
                    UItem::TypedefEnum { .. } | UItem::TypedefStruct { .. } => {
                        self.collect_types_into(
                            std::slice::from_ref(item),
                            &mut info.scope,
                            &mut info.types,
                            &mut info.enums,
                        );
                    }
                    UItem::Function { func } => {
                        info.functions.insert(func.name.clone(), func.clone());
                    }
                    // A variable in a package is a global shared between
                    // modules, which is not hardware this models. Anything
                    // else was already reported when it was read.
                    _ => {}
                }
            }
            self.packages.push((package.name.clone(), info));
        }
    }

    /// Puts the packages' names in reach of a module, or of another package.
    ///
    /// Every package's names are bound under their qualified spelling —
    /// `defs::WIDTH` needs no import — and the ones imported are bound bare as
    /// well. This runs before the module's own declarations are bound, so
    /// those land on top and win, as they do in the language.
    fn seed(
        &self,
        imports: &[UImport],
        scope: &mut Scope,
        types: &mut TypeWidths,
        enums: &mut Vec<EnumType>,
        functions: &mut HashMap<String, UFunction>,
    ) {
        for (package, info) in &self.packages {
            let imported = |name: &str| {
                imports.iter().any(|import| {
                    import.package == *package
                        && import.item.as_deref().is_none_or(|item| item == name)
                })
            };
            for (name, value, width) in info.scope.iter_sized() {
                bind(scope, &qualified(package, name), value, width);
                if imported(name) {
                    bind(scope, name, value, width);
                }
            }
            for (name, width) in &info.types {
                types.insert(qualified(package, name), *width);
                if imported(name) {
                    types.insert(name.clone(), *width);
                }
            }
            for enumeration in &info.enums {
                enums.push(EnumType {
                    name: qualified(package, &enumeration.name),
                    ..enumeration.clone()
                });
                if imported(&enumeration.name) {
                    enums.push(enumeration.clone());
                }
            }
            for (name, func) in &info.functions {
                functions.insert(qualified(package, name), func.clone());
                if imported(name) {
                    functions.insert(name.clone(), func.clone());
                }
            }
        }
    }

    /// An import of a package none of the files declared: its names are
    /// unknown, and every use of them will be reported as such, so this says
    /// why once.
    fn report_missing_imports(&mut self, imports: &[UImport]) {
        for import in imports {
            if !self.packages.iter().any(|(name, _)| *name == import.package) {
                self.diags.push(
                    Diag::warning(
                        DiagCode::PackageNotFound,
                        format!(
                            "package `{}` is not in any file that was read, so what it \
                             declares is unknown here",
                            import.package
                        ),
                    )
                    .at(import.span),
                );
            }
        }
    }

    // ------------------------------------------------------- top detection ---

    /// The module nothing instantiates.
    ///
    /// A design usually has exactly one. Several means the file set holds more
    /// than one design, and the user has to say which — guessing would silently
    /// analyse the wrong thing.
    fn find_top(&mut self, requested: Option<&str>) -> Option<String> {
        if let Some(name) = requested {
            if self.by_name.contains_key(name) {
                return Some(name.to_string());
            }
            // The name the author wrote, for a design a tool rewrote: `--top
            // Control` means `lights_Control` when that is what Veryl called it.
            if let Some(module) =
                self.uir.modules.iter().find(|m| m.written.as_deref() == Some(name))
            {
                return Some(module.name.clone());
            }
            self.diags.push(Diag::error(
                DiagCode::TopNotFound,
                format!("no module named `{name}` in the given sources"),
            ));
            return None;
        }

        let roots: Vec<&UModule> = {
            let names = candidate_tops(self.uir);
            self.uir.modules.iter().filter(|m| names.contains(&m.name)).collect()
        };

        match roots.as_slice() {
            [] => {
                self.diags.push(Diag::error(
                    DiagCode::TopNotFound,
                    "every module is instantiated by another, so there is no top; \
                     the sources may describe a cycle",
                ));
                None
            }
            [only] => Some(only.name.clone()),
            many => {
                let names: Vec<&str> = many.iter().map(|m| m.name.as_str()).collect();
                self.diags.push(Diag::error(
                    DiagCode::TopAmbiguous,
                    format!(
                        "{} modules are not instantiated by anything ({}); pass --top to choose",
                        names.len(),
                        names.join(", ")
                    ),
                ));
                None
            }
        }
    }

    // ------------------------------------------------------------- modules ---
    // (`candidate_tops` sits at the end of the file, since it is the one piece
    // of this pass a caller outside the crate needs.)

    fn elaborate_module(
        &mut self,
        name: &str,
        overrides: &[(String, i64)],
        bindings: &[IfaceBinding],
        instantiation_span: Span,
    ) -> Option<ModuleId> {
        let Some(umodule) = self.by_name.get(name).copied() else {
            // A module we have no source for: an IP stub, or a file the user
            // forgot to list. Drawn as an opaque box rather than dropped.
            self.diags.push(
                Diag::warning(
                    DiagCode::ModuleNotFound,
                    format!("no source for module `{name}`; treating it as a black box"),
                )
                .at(instantiation_span),
            );
            return Some(self.alloc_blackbox(name, instantiation_span));
        };

        let params = self.evaluate_params(umodule, overrides);
        let key: SpecKey = (
            name.to_string(),
            params.iter().map(|p| (p.name.clone(), p.value)).collect(),
            bindings.to_vec(),
        );

        if let Some(id) = self.cache.get(&key) {
            return Some(*id);
        }
        if self.in_progress.contains(&key) || self.depth >= MAX_DEPTH {
            self.diags.push(
                Diag::error(
                    DiagCode::RecursionLimitExceeded,
                    format!("module `{name}` instantiates itself, directly or indirectly"),
                )
                .at(instantiation_span),
            );
            return None;
        }

        self.in_progress.insert(key.clone());
        self.depth += 1;
        let module = self.build_module(umodule, params, &key);
        self.depth -= 1;
        self.in_progress.remove(&key);

        let id = self.modules.alloc(module);
        self.cache.insert(key, id);
        Some(id)
    }

    fn alloc_blackbox(&mut self, name: &str, span: Span) -> ModuleId {
        if let Some(id) = self.cache.get(&(name.to_string(), Vec::new(), Vec::new())) {
            return *id;
        }
        let module = Module {
            name: name.to_string(),
            base_name: name.to_string(),
            written: None,
            params: Vec::new(),
            enums: Vec::new(),
            bundles: Vec::new(),
            iface_insts: Vec::new(),
            ports: Vec::new(),
            nets: Arena::new(),
            insts: Vec::new(),
            procs: Vec::new(),
            skipped: Vec::new(),
            is_blackbox: true,
            span,
        };
        let id = self.modules.alloc(module);
        self.cache.insert((name.to_string(), Vec::new(), Vec::new()), id);
        id
    }

    /// Evaluates the header parameters in declaration order, so a later default
    /// may refer to an earlier parameter, with instantiation overrides applied.
    fn evaluate_params(&mut self, umodule: &UModule, overrides: &[(String, i64)]) -> Vec<Param> {
        self.evaluate_param_list(
            &umodule.params,
            &umodule.imports,
            overrides,
            &umodule.name,
            umodule.span,
        )
    }

    /// The same for any parameter list: a module's, or an interface's.
    fn evaluate_param_list(
        &mut self,
        uparams: &[UParam],
        imports: &[UImport],
        overrides: &[(String, i64)],
        owner: &str,
        owner_span: Span,
    ) -> Vec<Param> {
        let mut scope = Scope::new();
        // `parameter int W = defs::WIDTH` — and, with an import, plain `WIDTH`.
        self.seed(
            imports,
            &mut scope,
            &mut TypeWidths::new(),
            &mut Vec::new(),
            &mut HashMap::new(),
        );
        let mut params = Vec::new();

        for uparam in uparams {
            let overridden =
                overrides.iter().find(|(name, _)| *name == uparam.name).map(|(_, value)| *value);

            let value = match overridden {
                Some(value) if !uparam.is_local => Some(value),
                _ => match &uparam.default {
                    Some(default) => self.try_eval_param(&uparam.name, default, &scope),
                    None => {
                        self.diags.push(
                            Diag::error(
                                DiagCode::ParamUnknown,
                                format!(
                                    "parameter `{}` has no default and was not given a value",
                                    uparam.name
                                ),
                            )
                            .at(uparam.span),
                        );
                        None
                    }
                },
            };

            let value = value.unwrap_or(0);
            // The declared width goes into scope too: a concatenation is where
            // `parameter logic [5:0] DT` differs from a plain integer.
            let width =
                uparam.packed.as_ref().map(|range| self.width_of(Some(range), &scope, uparam.span));
            match width {
                Some(width) => scope.bind_sized(&uparam.name, value, width),
                None => scope.bind(&uparam.name, value),
            }
            params.push(Param {
                name: uparam.name.clone(),
                written: uparam.written.clone(),
                value,
                is_local: uparam.is_local,
                width,
                span: uparam.span,
            });
        }

        // An override naming a parameter the module does not have is a typo
        // that would otherwise do nothing at all.
        for (name, _) in overrides {
            if !uparams.iter().any(|p| p.name == *name) {
                self.diags.push(
                    Diag::warning(
                        DiagCode::ParamUnknown,
                        format!("`{owner}` has no parameter `{name}`"),
                    )
                    .at(owner_span),
                );
            }
        }

        params
    }

    fn build_module(&mut self, umodule: &UModule, params: Vec<Param>, key: &SpecKey) -> Module {
        let mut scope = Scope::new();
        let mut types = TypeWidths::new();
        let mut enums = Vec::new();
        let mut functions = HashMap::new();
        // What the packages give this module, before its own declarations, so
        // that those land on top.
        self.seed(&umodule.imports, &mut scope, &mut types, &mut enums, &mut functions);
        self.report_missing_imports(&umodule.imports);
        for param in &params {
            // With its width, where there was one. Rebuilding the scope from
            // values alone silently dropped every declared width, and a
            // concatenation of parameters then could not be folded at all.
            match param.width {
                Some(width) => scope.bind_sized(&param.name, param.value, width),
                None => scope.bind(&param.name, param.value),
            }
        }

        // Typedefs first. Their members are constants that widths, ports and
        // process bodies may all refer to, and their widths are what sizes a
        // declaration of that type — so nothing else can be resolved until they
        // are known.
        types.extend(self.collect_types(&umodule.items, &mut scope));
        // The same walk again, for the member names: a state machine read back
        // as numbers is a state machine nobody can check against the source.
        enums.extend(self.collect_enums(&umodule.items, &mut scope.clone()));

        // Functions are collected up front because a call may appear above the
        // declaration, and because a copy of the body is needed at each call
        // site rather than at the point it was written.
        collect_functions(&umodule.items, &mut functions);

        let mut body = Body {
            nets: Arena::new(),
            names: vec![HashMap::new()],
            insts: Vec::new(),
            procs: Vec::new(),
            skipped: Vec::new(),
            extra_params: Vec::new(),
            functions,
            call_sites: 0,
            types: types.clone(),
            package_scopes: self
                .packages
                .iter()
                .map(|(name, info)| (name.clone(), info.scope.clone()))
                .collect(),
            ifaces: HashMap::new(),
            bundles: Vec::new(),
            iface_insts: Vec::new(),
        };

        // Ports become nets first: everything else in the module may refer to
        // them, and a port with no net would break the Port -> NetId link.
        let ports = self.build_ports(umodule, &scope, &types, &key.2, &mut body);

        self.walk_items(&umodule.items, &scope, &types, "", None, &mut body);

        let mut params = params;
        params.extend(body.extra_params);

        Module {
            name: specialised_name(&umodule.name, key),
            base_name: umodule.name.clone(),
            written: umodule.written.clone(),
            params,
            enums,
            bundles: body.bundles,
            iface_insts: body.iface_insts,
            ports,
            nets: body.nets,
            insts: body.insts,
            procs: body.procs,
            skipped: body.skipped,
            is_blackbox: false,
            span: umodule.span,
        }
    }

    /// Evaluates every `typedef enum` in a module: binds its members as
    /// constants, and records the width a declaration of that type gets.
    ///
    /// Members auto-number from zero, each one after an explicit value carrying
    /// on from it. That is the SystemVerilog rule, and the reason `{ IDLE, RUN,
    /// DONE }` needs no values written at all.
    fn collect_types(&mut self, items: &[UItem], scope: &mut Scope) -> TypeWidths {
        let mut widths = TypeWidths::default();
        let mut enums = Vec::new();
        self.collect_types_into(items, scope, &mut widths, &mut enums);
        widths
    }

    /// The same walk, keeping the members as well as the widths.
    fn collect_enums(&mut self, items: &[UItem], scope: &mut Scope) -> Vec<EnumType> {
        let mut widths = TypeWidths::new();
        let mut enums = Vec::new();
        self.collect_types_into(items, scope, &mut widths, &mut enums);
        enums
    }

    fn collect_types_into(
        &mut self,
        items: &[UItem],
        scope: &mut Scope,
        widths: &mut TypeWidths,
        enums: &mut Vec<EnumType>,
    ) {
        for item in items {
            match item {
                UItem::TypedefEnum { name, packed, members, span } => {
                    // A bare `enum` is an `int`, so 32 bits, per IEEE 1800.
                    let width = match packed {
                        Some(range) => self.width_of(Some(range), scope, *span),
                        None => 32,
                    };
                    widths.insert(name.clone(), width);

                    let mut next = 0i64;
                    let mut collected = Vec::new();
                    for member in members {
                        let value = match &member.value {
                            Some(expr) => self.try_eval(expr, scope).unwrap_or(next),
                            None => next,
                        };
                        scope.bind(&member.name, value);
                        collected.push(EnumMember {
                            name: member.name.clone(),
                            written: member.written.clone(),
                            value,
                            span: member.span,
                        });
                        next = value + 1;
                    }
                    enums.push(EnumType { name: name.clone(), width, members: collected });
                }
                UItem::TypedefStruct { name, members, .. } => {
                    // A packed struct is a bit vector with names for its parts,
                    // so its width is its members' widths added up. A member
                    // of a type declared above it is sized through `widths`,
                    // which is why the order of the typedefs is kept.
                    let mut width = 0;
                    for member in members {
                        width += self.declared_width(
                            member.packed.as_ref(),
                            Some(member.net_type),
                            member.type_name.as_ref(),
                            widths,
                            scope,
                            member.span,
                        );
                    }
                    widths.insert(name.clone(), width);
                }
                // A typedef inside a generate block is visible outside it in
                // practice, and chasing the exceptions costs more than it saves.
                UItem::GenerateFor { body, .. } => {
                    self.collect_types_into(body, scope, widths, enums)
                }
                UItem::GenerateIf { then_items, else_items, .. } => {
                    self.collect_types_into(then_items, scope, widths, enums);
                    self.collect_types_into(else_items, scope, widths, enums);
                }
                UItem::GenerateBlock { items, .. } => {
                    self.collect_types_into(items, scope, widths, enums)
                }
                _ => {}
            }
        }
    }

    /// The width a declaration gets: from its own range, or from its type.
    fn declared_width(
        &mut self,
        packed: Option<&URange>,
        net_type: Option<UNetType>,
        type_name: Option<&String>,
        types: &TypeWidths,
        scope: &Scope,
        span: Span,
    ) -> u32 {
        if let Some(range) = packed {
            return self.width_of(Some(range), scope, span);
        }
        // `int x;` carries its width in the keyword, not in a range.
        if let Some(width) = net_type.and_then(UNetType::intrinsic_width) {
            return width;
        }
        let Some(name) = type_name else { return 1 };
        match types.get(name.as_str()) {
            Some(width) => *width,
            None => {
                // A type with no typedef in reach: imported from a package, or a
                // struct. One bit is a placeholder, and saying so is the whole
                // point — a silently mis-sized signal is worse than a warning.
                self.diags.push(
                    Diag::warning(
                        DiagCode::UnknownDataType,
                        format!(
                            "type `{name}` is not in the subset, so the width of this \
                             declaration is unknown; it is modelled as one bit for now"
                        ),
                    )
                    .at(span),
                );
                1
            }
        }
    }

    // --------------------------------------------------------------- ports ---

    fn build_ports(
        &mut self,
        umodule: &UModule,
        scope: &Scope,
        types: &TypeWidths,
        bindings: &[IfaceBinding],
        body: &mut Body,
    ) -> Vec<Port> {
        // A non-ANSI header gives only names; the directions and widths arrive
        // as body declarations, so gather those first.
        let mut declared: HashMap<&str, (&PortDir, Option<&URange>)> = HashMap::new();
        for item in &umodule.items {
            if let UItem::PortDecl { name, dir, packed, .. } = item {
                declared.insert(name.as_str(), (dir, packed.as_ref()));
            }
        }

        let mut ports = Vec::new();
        for uport in &umodule.ports {
            if let Some(iface_port) = &uport.iface {
                self.unfold_bundle(uport, iface_port, bindings, &mut ports, body);
                continue;
            }
            let body_decl = declared.get(uport.name.as_str());
            let dir = uport.dir.or(body_decl.map(|(dir, _)| **dir)).unwrap_or_else(|| {
                self.diags.push(
                    Diag::warning(
                        DiagCode::PortNotFound,
                        format!("port `{}` has no direction; assuming input", uport.name),
                    )
                    .at(uport.span),
                );
                PortDir::Input
            });
            let packed = uport.packed.as_ref().or(body_decl.and_then(|(_, p)| *p));
            let width = self.declared_width(
                packed,
                uport.net_type,
                uport.type_name.as_ref(),
                types,
                scope,
                uport.span,
            );
            let kind = self.net_kind_of(uport.unpacked.as_ref(), scope, uport.span);

            let net = body.declare_net(Net {
                name: uport.name.clone(),
                written: uport.written.clone(),
                width,
                kind,
                type_name: uport.type_name.clone(),
                synthesised: false,
                span: uport.span,
            });
            ports.push(Port {
                name: uport.name.clone(),
                written: uport.written.clone(),
                dir,
                net,
                span: uport.span,
            });
        }
        ports
    }

    fn width_of(&mut self, packed: Option<&URange>, scope: &Scope, span: Span) -> u32 {
        let Some(range) = packed else { return 1 };
        let msb = self.try_eval(&range.msb, scope);
        let lsb = self.try_eval(&range.lsb, scope);
        match (msb, lsb) {
            (Some(msb), Some(lsb)) => (msb - lsb).unsigned_abs() as u32 + 1,
            // The range failed to evaluate and was already reported; one bit is
            // the least misleading placeholder.
            _ => {
                let _ = span;
                1
            }
        }
    }

    fn net_kind_of(&mut self, unpacked: Option<&URange>, scope: &Scope, span: Span) -> NetKind {
        let Some(range) = unpacked else { return NetKind::Logic };
        let msb = self.try_eval(&range.msb, scope);
        let lsb = self.try_eval(&range.lsb, scope);
        match (msb, lsb) {
            (Some(msb), Some(lsb)) => {
                NetKind::Memory { depth: (msb - lsb).unsigned_abs() as u32 + 1 }
            }
            _ => {
                let _ = span;
                NetKind::Logic
            }
        }
    }

    // --------------------------------------------------------------- items ---

    /// Walks a scope's items, unrolling generate constructs as it goes.
    ///
    /// Declarations are processed before instances so that an instance may
    /// connect to a net declared below it, which SystemVerilog allows.
    #[allow(clippy::too_many_arguments)]
    fn walk_items(
        &mut self,
        items: &[UItem],
        scope: &Scope,
        types: &TypeWidths,
        prefix: &str,
        index: Option<i64>,
        body: &mut Body,
    ) {
        let mut scope = scope.clone();

        for item in items {
            match item {
                UItem::Param { param } => {
                    let value = param
                        .default
                        .as_ref()
                        .and_then(|default| self.try_eval(default, &scope))
                        .unwrap_or(0);
                    let width = param
                        .packed
                        .as_ref()
                        .map(|range| self.width_of(Some(range), &scope, param.span));
                    match width {
                        Some(width) => scope.bind_sized(&param.name, value, width),
                        None => scope.bind(&param.name, value),
                    }
                    body.extra_params.push(Param {
                        name: param.name.clone(),
                        written: param.written.clone(),
                        value,
                        is_local: true,
                        width,
                        span: param.span,
                    });
                }
                // An interface instance is a set of nets, so it is declared
                // here with the rest, ahead of the logic that names them.
                UItem::Inst { inst } if self.interfaces.contains_key(inst.module_name.as_str()) => {
                    self.instantiate_interface(inst, &scope, prefix, body);
                }
                UItem::Net { net } => {
                    let width = self.declared_width(
                        net.packed.as_ref(),
                        Some(net.net_type),
                        net.type_name.as_ref(),
                        types,
                        &scope,
                        net.span,
                    );
                    let kind = self.net_kind_of(net.unpacked.as_ref(), &scope, net.span);
                    let name = format!("{prefix}{}", net.name);
                    let written = net.written.as_ref().map(|written| format!("{prefix}{written}"));
                    let id = body.declare_net(Net {
                        name,
                        written,
                        width,
                        kind,
                        type_name: net.type_name.clone(),
                        synthesised: false,
                        span: net.span,
                    });
                    body.bind_name(&net.name, id);
                }
                _ => {}
            }
        }

        for item in items {
            match item {
                // Already handled: typedefs by `collect_types`, the rest by the
                // declaration pass above.
                UItem::Param { .. }
                | UItem::Net { .. }
                | UItem::PortDecl { .. }
                | UItem::TypedefEnum { .. }
                | UItem::TypedefStruct { .. } => {}
                UItem::Inst { inst } => {
                    if !self.interfaces.contains_key(inst.module_name.as_str()) {
                        self.instantiate(inst, &scope, prefix, body);
                    }
                }
                UItem::GenerateFor { genvar, init, cond, step, body: loop_body, span } => self
                    .unroll_for(
                        genvar, init, cond, step, loop_body, *span, &scope, types, prefix, body,
                    ),
                UItem::GenerateIf { cond, then_items, else_items, span } => {
                    match self.try_eval(cond, &scope) {
                        Some(0) => self.walk_items(else_items, &scope, types, prefix, index, body),
                        Some(_) => self.walk_items(then_items, &scope, types, prefix, index, body),
                        // The condition was already reported; taking neither
                        // branch is better than guessing which one.
                        None => {
                            let _ = span;
                        }
                    }
                }
                UItem::GenerateBlock { label, items, span } => {
                    let nested = match (label, index) {
                        (Some(label), Some(i)) => format!("{prefix}{label}[{i}]."),
                        (Some(label), None) => format!("{prefix}{label}."),
                        (None, _) => prefix.to_string(),
                    };
                    body.push_scope();
                    self.walk_items(items, &scope, types, &nested, None, body);
                    body.pop_scope();
                    let _ = span;
                }
                // Already collected, and a declaration on its own is not
                // hardware: the logic appears where the function is called.
                UItem::Function { .. } => {}
                UItem::Defparam { span, .. } => {
                    body.skipped.push(Skipped { construct: "defparam".into(), span: *span });
                }
                UItem::Unsupported { construct, span } => {
                    body.skipped.push(Skipped { construct: construct.clone(), span: *span });
                }
                UItem::Assign { lhs, rhs, span } => {
                    let process = ProcBuilder {
                        scope: &scope,
                        names: body,
                        diags: &mut self.diags,
                        prelude: Vec::new(),
                        depth: 0,
                    }
                    .continuous_assign(lhs, rhs, *span);
                    body.push_process(process);
                }
                UItem::Proc { kind, body: proc_body, span } => {
                    let process = ProcBuilder {
                        scope: &scope,
                        names: body,
                        diags: &mut self.diags,
                        prelude: Vec::new(),
                        depth: 0,
                    }
                    .process(kind, proc_body, *span);
                    body.push_process(process);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn unroll_for(
        &mut self,
        genvar: &str,
        init: &UExpr,
        cond: &UExpr,
        step: &UExpr,
        loop_body: &[UItem],
        span: Span,
        scope: &Scope,
        types: &TypeWidths,
        prefix: &str,
        body: &mut Body,
    ) {
        let Some(mut value) = self.try_eval(init, scope) else { return };
        let mut iterations = 0i64;

        loop {
            let mut iteration_scope = scope.child();
            iteration_scope.bind(genvar, value);

            match self.try_eval(cond, &iteration_scope) {
                Some(0) => break,
                Some(_) => {}
                None => return,
            }

            iterations += 1;
            if iterations > MAX_GENERATE_ITERATIONS {
                self.diags.push(
                    Diag::error(
                        DiagCode::GenerateLimitExceeded,
                        format!(
                            "`for` generate exceeded {MAX_GENERATE_ITERATIONS} iterations; \
                             the condition is probably never false"
                        ),
                    )
                    .at(span),
                );
                return;
            }

            body.push_scope();
            self.walk_items(loop_body, &iteration_scope, types, prefix, Some(value), body);
            body.pop_scope();

            let Some(next) = self.try_eval(step, &iteration_scope) else { return };
            if next == value {
                self.diags.push(
                    Diag::error(
                        DiagCode::GenerateLimitExceeded,
                        format!("`for` generate never advances `{genvar}`"),
                    )
                    .at(span),
                );
                return;
            }
            value = next;
        }
    }

    // ----------------------------------------------------------- instances ---

    fn instantiate(&mut self, inst: &UInstance, scope: &Scope, prefix: &str, body: &mut Body) {
        let overrides = self.parameter_overrides(inst, scope);
        let bindings = self.interface_bindings(inst, body);
        let Some(of) = self.elaborate_module(&inst.module_name, &overrides, &bindings, inst.span)
        else {
            return;
        };

        // A black box has no source, so the only thing that knows its pins is
        // the instantiation in front of us. Take them from there, otherwise
        // every connection would be reported as naming a port that does not
        // exist and the box would be drawn with no edges at all.
        if self.modules[of].is_blackbox {
            self.adopt_blackbox_ports(of, inst);
        }

        // The child's ports are needed to resolve connections; copy the shape
        // out so the arena is free to grow while we work.
        let child_ports: Vec<ChildPort> = self.modules[of]
            .ports
            .iter()
            .map(|p| ChildPort {
                name: p.name.clone(),
                dir: p.dir,
                width: self.modules[of].nets[p.net].width,
            })
            .collect();

        // An interface port of the child is several ports here, connected as
        // one: the bundle says which of the child's ports it stands for.
        let child_bundles: Vec<(String, Vec<usize>)> = self.modules[of]
            .bundles
            .iter()
            .map(|bundle| {
                (bundle.name.clone(), bundle.ports.iter().map(|p| p.0 as usize).collect())
            })
            .collect();
        let conns = self.resolve_connections(inst, &child_ports, &child_bundles, scope, body);
        body.insts.push(Instance {
            name: format!("{prefix}{}", inst.name),
            written: inst.written.as_ref().map(|written| format!("{prefix}{written}")),
            of,
            conns,
            span: inst.span,
        });
    }

    /// Gives a black box the pins the instantiation names.
    ///
    /// Direction and width are genuinely unknown — the module is flagged
    /// `is_blackbox` precisely so a consumer knows not to trust anything inside
    /// it — so they are recorded as one-bit `inout` rather than guessed at.
    /// Instantiating the same black box twice with different connections adds
    /// the union of their pins.
    fn adopt_blackbox_ports(&mut self, id: ModuleId, inst: &UInstance) {
        let names: Vec<String> = match &inst.conns {
            UConns::Named { conns } | UConns::Wildcard { conns } => {
                conns.iter().map(|conn| conn.port.clone()).collect()
            }
            // Positional connections carry no names at all.
            UConns::Positional { values } => {
                (0..values.len()).map(|index| format!("p{index}")).collect()
            }
            UConns::Empty => Vec::new(),
        };

        let module = &mut self.modules[id];
        let span = module.span;
        for name in names {
            if module.ports.iter().any(|port| port.name == name) {
                continue;
            }
            let net = module.nets.alloc(Net {
                name: name.clone(),
                written: None,
                width: 1,
                kind: NetKind::Logic,
                type_name: None,
                synthesised: false,
                span,
            });
            module.ports.push(Port { name, written: None, dir: PortDir::Inout, net, span });
        }
    }

    fn parameter_overrides(&mut self, inst: &UInstance, scope: &Scope) -> Vec<(String, i64)> {
        match &inst.param_overrides {
            UParamOverrides::Empty => Vec::new(),
            UParamOverrides::Named { values } => values
                .iter()
                .filter_map(|(name, expr)| {
                    self.try_eval_param(name, expr, scope).map(|value| (name.clone(), value))
                })
                .collect(),
            UParamOverrides::Positional { values } => {
                // Positional overrides bind to the child's parameters in
                // declaration order, so the child has to be looked up by name.
                let overridable = |params: &[UParam]| -> Vec<String> {
                    params.iter().filter(|p| !p.is_local).map(|p| p.name.clone()).collect()
                };
                let names: Vec<String> = self
                    .by_name
                    .get(inst.module_name.as_str())
                    .map(|m| overridable(&m.params))
                    .or_else(|| {
                        self.interfaces
                            .get(inst.module_name.as_str())
                            .map(|i| overridable(&i.params))
                    })
                    .unwrap_or_default();
                if values.len() > names.len() {
                    self.diags.push(
                        Diag::warning(
                            DiagCode::TooManyConnections,
                            format!(
                                "{} parameter values given but `{}` declares {}",
                                values.len(),
                                inst.module_name,
                                names.len()
                            ),
                        )
                        .at(inst.span),
                    );
                }
                names
                    .into_iter()
                    .zip(values)
                    .filter_map(|(name, expr)| {
                        self.try_eval_param(&name, expr, scope).map(|value| (name, value))
                    })
                    .collect()
            }
        }
    }

    fn resolve_connections(
        &mut self,
        inst: &UInstance,
        child_ports: &[ChildPort],
        child_bundles: &[(String, Vec<usize>)],
        scope: &Scope,
        body: &mut Body,
    ) -> Vec<Conn> {
        let mut resolved: Vec<Conn> = Vec::new();
        let mut bound: HashSet<usize> = HashSet::new();

        let port_index = |name: &str| child_ports.iter().position(|p| p.name == name);

        match &inst.conns {
            UConns::Empty => {}
            UConns::Positional { values } => {
                // A bundle is several of the child's ports, and a position
                // names one: the two cannot be matched up without guessing.
                if !child_bundles.is_empty() {
                    self.diags.push(
                        Diag::warning(
                            DiagCode::UnsupportedConstruct,
                            format!(
                                "`{}` has interface ports, which positional connections cannot \
                                 be matched to; connect `{}` by name",
                                inst.module_name, inst.name
                            ),
                        )
                        .at(inst.span),
                    );
                    return resolved;
                }
                if values.len() > child_ports.len() {
                    self.diags.push(
                        Diag::warning(
                            DiagCode::TooManyConnections,
                            format!(
                                "{} connections given but `{}` has {} ports",
                                values.len(),
                                inst.module_name,
                                child_ports.len()
                            ),
                        )
                        .at(inst.span),
                    );
                }
                for (index, value) in values.iter().enumerate() {
                    if index >= child_ports.len() {
                        break;
                    }
                    let Some(expr) = value else { continue };
                    let port = &child_ports[index];
                    if let Some(net_ref) = self.net_ref_of(expr, scope, body, &inst.name, port) {
                        // A positional connection is written as bare
                        // expression, so that is what an edit replaces.
                        let id = PortId(index as u32);
                        resolved.push(Conn::new(id, net_ref, expr.span));
                        bound.insert(index);
                    }
                }
            }
            UConns::Named { conns } | UConns::Wildcard { conns } => {
                for conn in conns {
                    if let Some((_, indices)) = child_bundles.iter().find(|(b, _)| *b == conn.port)
                    {
                        self.connect_bundle(
                            inst,
                            conn,
                            indices,
                            child_ports,
                            body,
                            &mut resolved,
                            &mut bound,
                        );
                        continue;
                    }
                    let Some(index) = port_index(&conn.port) else {
                        self.diags.push(
                            Diag::warning(
                                DiagCode::PortNotFound,
                                format!("`{}` has no port `{}`", inst.module_name, conn.port),
                            )
                            .at(conn.span),
                        );
                        continue;
                    };
                    // `.port()` is an explicit "leave this open" and is not a
                    // mistake, so it binds nothing and warns about nothing.
                    // Marked bound all the same: a port written open is not a
                    // port nobody mentioned, and warning about it would be
                    // reporting the author's own decision back to them.
                    let Some(expr) = &conn.expr else {
                        bound.insert(index);
                        continue;
                    };
                    let port = &child_ports[index];
                    if let Some(net_ref) = self.net_ref_of(expr, scope, body, &inst.name, port) {
                        let id = PortId(index as u32);
                        resolved.push(Conn::new(id, net_ref, conn.span));
                        bound.insert(index);
                    }
                }

                if let UConns::Wildcard { .. } = &inst.conns {
                    self.bind_wildcard(inst, child_ports, &mut bound, &mut resolved, body);
                }
            }
        }

        for (index, port) in child_ports.iter().enumerate() {
            let name = &port.name;
            if !bound.contains(&index) {
                self.diags.push(
                    Diag::warning(
                        DiagCode::PortUnconnected,
                        format!("port `{}` of `{}` is not connected", name, inst.name),
                    )
                    .at(inst.span),
                );
            }
        }

        resolved.sort_by_key(|conn| conn.port);
        resolved
    }

    /// Which interface each interface port of a child is being given.
    ///
    /// Read off the connections before the child is elaborated, because the
    /// child's ports depend on it: `interface c` is whatever `.c(bus)` says.
    fn interface_bindings(&self, inst: &UInstance, body: &Body) -> Vec<IfaceBinding> {
        let Some(child) = self.by_name.get(inst.module_name.as_str()) else { return Vec::new() };
        let mut out = Vec::new();
        for uport in &child.ports {
            let Some(iface_port) = &uport.iface else { continue };
            let named = match &inst.conns {
                UConns::Named { conns } | UConns::Wildcard { conns } => conns
                    .iter()
                    .find(|conn| conn.port == uport.name)
                    .and_then(|conn| conn.expr.as_ref())
                    .and_then(ident_name),
                _ => None,
            };
            // `.*` gives the port the interface of the same name, if there is one.
            let actual = named.or_else(|| {
                matches!(inst.conns, UConns::Wildcard { .. }).then(|| uport.name.clone())
            });
            let Some(actual) = actual else { continue };
            let Some(found) = body.ifaces.get(&actual) else { continue };
            if found.interface.is_empty() {
                continue;
            }
            out.push(IfaceBinding {
                port: uport.name.clone(),
                interface: found.interface.clone(),
                params: found.params.clone(),
                modport: iface_port.modport.clone().or_else(|| found.modport.clone()),
            });
        }
        out.sort();
        out
    }

    /// `.bus(b)` where `bus` is an interface port of the child: each of the
    /// ports it unfolded into is wired to the like-named signal of `b`.
    #[allow(clippy::too_many_arguments)]
    fn connect_bundle(
        &mut self,
        inst: &UInstance,
        conn: &UConn,
        indices: &[usize],
        child_ports: &[ChildPort],
        body: &mut Body,
        resolved: &mut Vec<Conn>,
        bound: &mut HashSet<usize>,
    ) {
        // `.bus()` is an explicit "leave this open", as for a wire.
        let Some(expr) = &conn.expr else {
            bound.extend(indices.iter().copied());
            return;
        };
        let Some(actual) = ident_name(expr) else {
            self.diags.push(
                Diag::warning(
                    DiagCode::UnsupportedExpr,
                    format!(
                        "`{expr}` is not an interface; port `{}` of `{}` takes one, and is left \
                         unconnected",
                        conn.port, inst.name
                    ),
                )
                .at(conn.span),
            );
            return;
        };
        let Some(found) = body.ifaces.get(&actual).cloned() else {
            self.diags.push(
                Diag::warning(
                    DiagCode::PortNotFound,
                    format!(
                        "`{actual}` is not an interface instance or interface port here, so \
                         port `{}` of `{}` is left unconnected",
                        conn.port, inst.name
                    ),
                )
                .at(conn.span),
            );
            return;
        };
        // An interface nobody named: the parent's generic port that had nothing
        // connected, already reported there.
        if found.interface.is_empty() {
            bound.extend(indices.iter().copied());
            return;
        }
        for &index in indices {
            let port = &child_ports[index];
            // The child's port is `bus.valid`; the signal is `valid`.
            let member = port.name.split_once('.').map_or(port.name.as_str(), |(_, m)| m);
            match found.members.iter().find(|(name, _)| name == member) {
                Some((_, net)) => {
                    resolved.push(Conn::new(
                        PortId(index as u32),
                        NetRef::Full { net: *net },
                        conn.span,
                    ));
                    bound.insert(index);
                }
                None => self.diags.push(
                    Diag::warning(
                        DiagCode::PortNotFound,
                        format!(
                            "`{actual}` (`{}`) has no signal `{member}` for port `{}` of `{}`",
                            found.interface, port.name, inst.name
                        ),
                    )
                    .at(conn.span),
                ),
            }
        }
    }

    /// `.*` binds every remaining port to a net of the same name.
    fn bind_wildcard(
        &mut self,
        inst: &UInstance,
        child_ports: &[ChildPort],
        bound: &mut HashSet<usize>,
        resolved: &mut Vec<Conn>,
        body: &mut Body,
    ) {
        for (index, port) in child_ports.iter().enumerate() {
            if bound.contains(&index) {
                continue;
            }
            let name = &port.name;
            match body.lookup(name) {
                Some(net) => {
                    // Nothing was written for this port: `.*` bound it. There
                    // is no text to replace, and `Span::UNKNOWN` is how the
                    // editor is told that changing it means inserting an
                    // explicit connection instead.
                    let id = PortId(index as u32);
                    let net = NetRef::Full { net };
                    let conn = Conn::new(id, net, Span::UNKNOWN);
                    resolved.push(conn);
                    bound.insert(index);
                }
                None => {
                    self.diags.push(
                        Diag::warning(
                            DiagCode::WildcardNoMatch,
                            format!(
                                "`.*` on `{}` found no net named `{name}` to bind port `{name}`",
                                inst.name
                            ),
                        )
                        .at(inst.span),
                    );
                }
            }
        }
    }

    /// Resolves a connection expression to something a port can be wired to.
    fn net_ref_of(
        &mut self,
        expr: &UExpr,
        scope: &Scope,
        body: &mut Body,
        inst_name: &str,
        port: &ChildPort,
    ) -> Option<NetRef> {
        match &*expr.kind {
            UExprKind::Ident { name } => {
                Some(NetRef::Full { net: body.lookup_or_create(name, expr.span, &mut self.diags) })
            }
            UExprKind::Int { .. } | UExprKind::Sized { .. } => {
                let value = self.try_eval(expr, scope)?;
                // An unsized constant in a connection takes 32 bits, per IEEE.
                let width = match &*expr.kind {
                    UExprKind::Sized { value } => value.width,
                    _ => 32,
                };
                Some(NetRef::Const { value: ConstBits::from_i64(width, value) })
            }
            UExprKind::Range { base, msb, lsb } => {
                let name = ident_name(base)?;
                let net = body.lookup_or_create(&name, expr.span, &mut self.diags);
                let msb = self.try_eval(msb, scope)?;
                let lsb = self.try_eval(lsb, scope)?;
                Some(NetRef::Slice { net, msb: msb.max(lsb) as u32, lsb: msb.min(lsb) as u32 })
            }
            UExprKind::Index { base, index } => {
                let name = ident_name(base)?;
                let net = body.lookup_or_create(&name, expr.span, &mut self.diags);
                let bit = self.try_eval(index, scope)? as u32;
                Some(NetRef::Slice { net, msb: bit, lsb: bit })
            }
            // Anything else is logic written in the connection itself:
            // `.rst(!rst_n)`, `.lp({p, n})`, `.sel(a ? b : c)`. It becomes the
            // net it would have been if someone had declared one, driven by a
            // continuous assignment — which is what synthesis does, and what
            // keeps the edge in the block diagram instead of an open pin.
            //
            // An expression that could not be assigned to can only be feeding
            // the instance, whatever the port's declared direction says — which
            // matters for a black box, whose adopted pins have no direction to
            // go on.
            _ if port.dir == PortDir::Input || !is_lvalue(expr) => {
                let name = body.unique_name(&format!("{inst_name}.{}", port.name));
                let net = body.declare_net(Net {
                    name: name.clone(),
                    written: None,
                    width: port.width,
                    kind: NetKind::Logic,
                    type_name: None,
                    synthesised: true,
                    span: expr.span,
                });
                let lhs = UExpr::new(UExprKind::Ident { name }, expr.span);
                let process = ProcBuilder {
                    scope,
                    names: body,
                    diags: &mut self.diags,
                    prelude: Vec::new(),
                    depth: 0,
                }
                .continuous_assign(&lhs, expr, expr.span);
                body.push_process(process);
                Some(NetRef::Full { net })
            }
            _ => {
                self.diags.push(
                    Diag::warning(
                        DiagCode::UnsupportedExpr,
                        format!(
                            "`{expr}` is not a form a port can be connected to yet; \
                             the port is left unconnected"
                        ),
                    )
                    .at(expr.span),
                );
                None
            }
        }
    }

    // ---------------------------------------------------------- interfaces ---

    /// `bus_if #(.W(8)) b ();` inside a module.
    ///
    /// An interface is a bundle of nets with logic of its own, so it is walked
    /// as a generate block named after the instance would be: its declarations
    /// become the nets `b.data`, `b.valid`, ..., its processes the module's
    /// own. The names are then bound with the prefix, which is how `b.valid`
    /// in the module's logic — and in a connection, `.bus(b)` — finds them.
    fn instantiate_interface(
        &mut self,
        inst: &UInstance,
        scope: &Scope,
        prefix: &str,
        body: &mut Body,
    ) {
        let Some(iface) = self.interfaces.get(inst.module_name.as_str()).copied() else { return };
        let overrides = self.parameter_overrides(inst, scope);
        let params = self.evaluate_param_list(
            &iface.params,
            &iface.imports,
            &overrides,
            &iface.name,
            inst.span,
        );
        let (iscope, types, functions) = self.interface_scope(iface, &params);

        let name = format!("{prefix}{}", inst.name);
        let bundle = format!("{name}.");
        let first = body.nets.len();

        // The interface's own ports — the `clk` of `interface bus_if (input
        // clk)` — are signals of the bundle too, wired up by the connections
        // written at the instantiation.
        let mut own_ports: Vec<(&UPort, NetId)> = Vec::new();
        for uport in &iface.ports {
            let width = self.declared_width(
                uport.packed.as_ref(),
                uport.net_type,
                uport.type_name.as_ref(),
                &types,
                &iscope,
                uport.span,
            );
            let kind = self.net_kind_of(uport.unpacked.as_ref(), &iscope, uport.span);
            let id = body.declare_net(Net {
                name: format!("{bundle}{}", uport.name),
                written: None,
                width,
                kind,
                type_name: uport.type_name.clone(),
                synthesised: false,
                span: uport.span,
            });
            own_ports.push((uport, id));
        }

        // Its functions and typedefs are in reach of its own logic, and of
        // nothing else.
        let saved_functions = std::mem::replace(&mut body.functions, functions);
        let saved_types = std::mem::replace(&mut body.types, types.clone());
        body.push_scope();
        for (uport, id) in &own_ports {
            body.bind_name(&uport.name, *id);
        }
        self.walk_items(&iface.items, &iscope, &types, &bundle, None, body);
        body.pop_scope();
        body.functions = saved_functions;
        body.types = saved_types;

        // Everything the walk declared is a signal of the bundle, and is
        // reachable from the module by its full name.
        let mut members = Vec::new();
        let mut nets = Vec::new();
        let declared: Vec<NetId> = body.nets.indices().skip(first).collect();
        for id in declared {
            let net = &body.nets[id];
            if net.synthesised {
                continue;
            }
            let Some(member) = net.name.strip_prefix(&bundle) else { continue };
            let (member, full) = (member.to_string(), net.name.clone());
            members.push((member, id));
            nets.push(id);
            body.bind_name(&full, id);
        }

        self.connect_interface_ports(inst, &own_ports, scope, body);

        let bindings =
            params.iter().filter(|p| !p.is_local).map(|p| (p.name.clone(), p.value)).collect();
        body.ifaces.insert(
            name.clone(),
            IfaceInst { interface: iface.name.clone(), params: bindings, modport: None, members },
        );
        body.iface_insts.push(IfaceInstance {
            name,
            written: inst.written.as_ref().map(|written| format!("{prefix}{written}")),
            interface: iface.name.clone(),
            nets,
            span: inst.span,
        });
    }

    /// The scope an interface's own declarations are read in: its imports,
    /// its parameters, its typedefs and its functions.
    fn interface_scope(
        &mut self,
        iface: &UInterface,
        params: &[Param],
    ) -> (Scope, TypeWidths, HashMap<String, UFunction>) {
        let mut scope = Scope::new();
        let mut types = TypeWidths::new();
        let mut enums = Vec::new();
        let mut functions = HashMap::new();
        self.seed(&iface.imports, &mut scope, &mut types, &mut enums, &mut functions);
        for param in params {
            bind(&mut scope, &param.name, param.value, param.width);
        }
        types.extend(self.collect_types(&iface.items, &mut scope));
        collect_functions(&iface.items, &mut functions);
        (scope, types, functions)
    }

    /// The connections written at an interface instantiation: `bus_if b
    /// (.clk(clk))` drives the bundle's own `clk` port, as an assignment.
    fn connect_interface_ports(
        &mut self,
        inst: &UInstance,
        own_ports: &[(&UPort, NetId)],
        scope: &Scope,
        body: &mut Body,
    ) {
        let mut pairs: Vec<(usize, &UExpr, Span)> = Vec::new();
        match &inst.conns {
            UConns::Empty => {}
            UConns::Positional { values } => {
                for (index, value) in values.iter().enumerate() {
                    if let Some(expr) = value {
                        pairs.push((index, expr, expr.span));
                    }
                }
            }
            UConns::Named { conns } | UConns::Wildcard { conns } => {
                for conn in conns {
                    let Some(expr) = &conn.expr else { continue };
                    match own_ports.iter().position(|(port, _)| port.name == conn.port) {
                        Some(index) => pairs.push((index, expr, conn.span)),
                        None => self.diags.push(
                            Diag::warning(
                                DiagCode::PortNotFound,
                                format!("`{}` has no port `{}`", inst.module_name, conn.port),
                            )
                            .at(conn.span),
                        ),
                    }
                }
            }
        }
        for (index, expr, span) in pairs {
            let Some((uport, net)) = own_ports.get(index) else {
                self.diags.push(
                    Diag::warning(
                        DiagCode::TooManyConnections,
                        format!(
                            "`{}` has {} port(s), and `{}` connects more",
                            inst.module_name,
                            own_ports.len(),
                            inst.name
                        ),
                    )
                    .at(span),
                );
                continue;
            };
            let here = UExpr::new(UExprKind::Ident { name: body.nets[*net].name.clone() }, span);
            let (lhs, rhs) = match uport.dir.unwrap_or(PortDir::Input) {
                PortDir::Input => (&here, expr),
                PortDir::Output => (expr, &here),
                PortDir::Inout => {
                    self.diags.push(
                        Diag::warning(
                            DiagCode::UnsupportedConstruct,
                            format!(
                                "`{}` is an inout port of `{}`, which a bundle cannot carry yet",
                                uport.name, inst.module_name
                            ),
                        )
                        .at(span),
                    );
                    continue;
                }
            };
            let process = ProcBuilder {
                scope,
                names: body,
                diags: &mut self.diags,
                prelude: Vec::new(),
                depth: 0,
            }
            .continuous_assign(lhs, rhs, span);
            body.push_process(process);
        }
    }

    /// `bus_if.slave s` on a module: one port per signal the modport lists,
    /// with the modport's direction — `s.data`, `s.valid`, ... — and the
    /// bundle that remembers they were one port.
    ///
    /// Which interface, and with which parameters, comes from what the
    /// instantiation connected when there was one, and from the declaration
    /// otherwise: a top module's `bus_if.slave s` is the interface with its
    /// defaults.
    fn unfold_bundle(
        &mut self,
        uport: &UPort,
        iface_port: &UIfacePort,
        bindings: &[IfaceBinding],
        ports: &mut Vec<Port>,
        body: &mut Body,
    ) {
        let bound = bindings.iter().find(|binding| binding.port == uport.name);
        let (iface_name, params, modport) = match (bound, &iface_port.interface) {
            (Some(binding), _) => (
                binding.interface.clone(),
                binding.params.clone(),
                iface_port.modport.clone().or_else(|| binding.modport.clone()),
            ),
            (None, Some(declared)) => (declared.clone(), Vec::new(), iface_port.modport.clone()),
            (None, None) => {
                self.diags.push(
                    Diag::warning(
                        DiagCode::PortUnconnected,
                        format!(
                            "`{}` is a generic `interface` port, and nothing here says which \
                             interface it is, so it has no signals",
                            uport.name
                        ),
                    )
                    .at(uport.span),
                );
                // Still a name: passing it on to a child, `.b(b)`, is not a
                // second mistake, and is not reported as one — here, or in
                // the parent, which finds an empty bundle to connect to.
                body.ifaces.insert(
                    uport.name.clone(),
                    IfaceInst {
                        interface: String::new(),
                        params: Vec::new(),
                        modport: iface_port.modport.clone(),
                        members: Vec::new(),
                    },
                );
                body.bundles.push(Bundle {
                    name: uport.name.clone(),
                    written: uport.written.clone(),
                    interface: String::new(),
                    modport: iface_port.modport.clone(),
                    ports: Vec::new(),
                    span: uport.span,
                });
                return;
            }
        };
        let Some(iface) = self.interfaces.get(iface_name.as_str()).copied() else {
            self.diags.push(
                Diag::warning(
                    DiagCode::ModuleNotFound,
                    format!(
                        "no source for interface `{iface_name}`; port `{}` is left without signals",
                        uport.name
                    ),
                )
                .at(uport.span),
            );
            return;
        };
        let signals = self.interface_signals(iface, &params);
        let directions: Option<Vec<(String, PortDir)>> =
            modport.as_ref().map(|modport| {
                match iface.modports.iter().find(|m| m.name == *modport) {
                    Some(found) => found.members.iter().map(|m| (m.name.clone(), m.dir)).collect(),
                    None => {
                        self.diags.push(
                            Diag::warning(
                                DiagCode::PortNotFound,
                                format!("`{iface_name}` has no modport `{modport}`"),
                            )
                            .at(uport.span),
                        );
                        Vec::new()
                    }
                }
            });

        let mut port_ids = Vec::new();
        let mut members = Vec::new();
        for signal in signals {
            // A modport lists the signals it exposes; a port with no modport
            // sees them all, and can drive or read any of them.
            let dir = match &directions {
                Some(list) => match list.iter().find(|(name, _)| *name == signal.name) {
                    Some((_, dir)) => *dir,
                    None => continue,
                },
                None => PortDir::Inout,
            };
            let name = format!("{}.{}", uport.name, signal.name);
            let written = (uport.written.is_some() || signal.written.is_some()).then(|| {
                format!(
                    "{}.{}",
                    uport.written.as_deref().unwrap_or(&uport.name),
                    signal.written.as_deref().unwrap_or(&signal.name)
                )
            });
            let net = body.declare_net(Net {
                name: name.clone(),
                written: written.clone(),
                width: signal.width,
                kind: signal.kind,
                type_name: signal.type_name.clone(),
                synthesised: false,
                // The net is the interface's signal, declared there; the port
                // is the module's, declared here.
                span: signal.span,
            });
            body.bind_name(&name, net);
            ports.push(Port { name, written, dir, net, span: uport.span });
            port_ids.push(PortId((ports.len() - 1) as u32));
            members.push((signal.name, net));
        }
        body.ifaces.insert(
            uport.name.clone(),
            IfaceInst {
                interface: iface_name.clone(),
                params: params.clone(),
                modport: modport.clone(),
                members,
            },
        );
        body.bundles.push(Bundle {
            name: uport.name.clone(),
            written: uport.written.clone(),
            interface: iface_name,
            modport,
            ports: port_ids,
            span: uport.span,
        });
    }

    /// The signals of an interface, sized with the given parameters: its own
    /// ports first, then what its body declares.
    fn interface_signals(&mut self, iface: &UInterface, params: &[(String, i64)]) -> Vec<Signal> {
        let evaluated = self.evaluate_param_list(
            &iface.params,
            &iface.imports,
            params,
            &iface.name,
            iface.span,
        );
        let (scope, types, _functions) = self.interface_scope(iface, &evaluated);
        let mut out = Vec::new();
        for uport in &iface.ports {
            let width = self.declared_width(
                uport.packed.as_ref(),
                uport.net_type,
                uport.type_name.as_ref(),
                &types,
                &scope,
                uport.span,
            );
            let kind = self.net_kind_of(uport.unpacked.as_ref(), &scope, uport.span);
            out.push(Signal {
                name: uport.name.clone(),
                written: uport.written.clone(),
                width,
                kind,
                type_name: uport.type_name.clone(),
                span: uport.span,
            });
        }
        self.collect_signals(&iface.items, &scope, &types, "", &mut out);
        out
    }

    /// The nets an item list declares, by name with any generate prefix, and
    /// nothing else: the declaration pass of `walk_items`, for reading an
    /// interface's shape without building its logic.
    ///
    /// A `generate for` is not unrolled here — its iterations are named by
    /// index, which is `walk_items`' job — so an interface that declares its
    /// signals in a loop unfolds into fewer ports than an instance of it has
    /// nets. Rare enough to leave, and the connection then says which signal
    /// is missing.
    fn collect_signals(
        &mut self,
        items: &[UItem],
        scope: &Scope,
        types: &TypeWidths,
        prefix: &str,
        out: &mut Vec<Signal>,
    ) {
        let mut scope = scope.clone();
        for item in items {
            match item {
                UItem::Param { param } => {
                    let value = param
                        .default
                        .as_ref()
                        .and_then(|default| self.try_eval(default, &scope))
                        .unwrap_or(0);
                    let width = param
                        .packed
                        .as_ref()
                        .map(|range| self.width_of(Some(range), &scope, param.span));
                    bind(&mut scope, &param.name, value, width);
                }
                UItem::Net { net } => {
                    let width = self.declared_width(
                        net.packed.as_ref(),
                        Some(net.net_type),
                        net.type_name.as_ref(),
                        types,
                        &scope,
                        net.span,
                    );
                    let kind = self.net_kind_of(net.unpacked.as_ref(), &scope, net.span);
                    out.push(Signal {
                        name: format!("{prefix}{}", net.name),
                        written: net.written.as_ref().map(|written| format!("{prefix}{written}")),
                        width,
                        kind,
                        type_name: net.type_name.clone(),
                        span: net.span,
                    });
                }
                UItem::GenerateBlock { label, items, .. } => {
                    let nested = match label {
                        Some(label) => format!("{prefix}{label}."),
                        None => prefix.to_string(),
                    };
                    self.collect_signals(items, &scope, types, &nested, out);
                }
                UItem::GenerateIf { cond, then_items, else_items, .. } => {
                    match self.try_eval(cond, &scope) {
                        Some(0) => self.collect_signals(else_items, &scope, types, prefix, out),
                        Some(_) => self.collect_signals(then_items, &scope, types, prefix, out),
                        None => {}
                    }
                }
                _ => {}
            }
        }
    }

    // -------------------------------------------------------------- shared ---

    /// Evaluates an expression, turning a failure into a diagnostic.
    ///
    /// A width or an array bound that will not evaluate is an error: the design
    /// that comes out is wrong, not merely incomplete.
    fn try_eval(&mut self, expr: &UExpr, scope: &Scope) -> Option<i64> {
        match eval(expr, scope) {
            Ok(value) => Some(value),
            Err(error) => {
                self.diags.push(Diag::error(code_for(&error), error.message()).at(error.span()));
                None
            }
        }
    }

    /// Evaluates a parameter value, where failing is a limitation rather than a
    /// fault.
    ///
    /// `PLLE2_BASE #(.BANDWIDTH("OPTIMIZED"))` is ordinary, correct RTL. The
    /// subset models integers only, so the string cannot be carried — but the
    /// design is not wrong, and calling it an error buries the real problems.
    /// Real evidence: 77 of 77 errors on a working design were string
    /// parameters passed to vendor primitives.
    fn try_eval_param(&mut self, name: &str, expr: &UExpr, scope: &Scope) -> Option<i64> {
        match eval(expr, scope) {
            Ok(value) => Some(value),
            Err(error) => {
                self.diags.push(
                    Diag::warning(
                        DiagCode::ParamNotConstant,
                        format!(
                            "parameter `{name}` is `{expr}`, which is not an integer;                              RTLScope models integer parameters only, so this one is ignored"
                        ),
                    )
                    .at(error.span()),
                );
                None
            }
        }
    }
}

/// The nets and names of one module under construction.
struct Body {
    nets: Arena<Net>,
    /// Innermost scope last. A generate block pushes one so its declarations do
    /// not escape.
    names: Vec<HashMap<String, NetId>>,
    insts: Vec<Instance>,
    procs: Vec<Process>,
    skipped: Vec<Skipped>,
    extra_params: Vec<Param>,
    /// Functions declared in this module, for inlining at their call sites.
    functions: HashMap<String, UFunction>,
    /// Distinguishes one call site from the next when naming their nets.
    call_sites: usize,
    /// Typedef widths, for sizing the locals of an inlined function.
    types: TypeWidths,
    /// Each package's constants, for the body of a function taken from one.
    package_scopes: HashMap<String, Scope>,
    /// The interfaces in reach by name: instances of them, and the module's
    /// own interface ports, each with the nets its signals became.
    ifaces: HashMap<String, IfaceInst>,
    bundles: Vec<Bundle>,
    iface_insts: Vec<IfaceInstance>,
}

impl NameResolver for Body {
    fn resolve_net(&mut self, name: &str, span: Span, diags: &mut Diagnostics) -> NetId {
        self.lookup_or_create(name, span, diags)
    }

    fn declare_local(&mut self, name: &str, width: u32, span: Span) -> NetId {
        self.declare_net(Net {
            name: name.to_string(),
            written: None,
            width,
            kind: NetKind::Logic,
            type_name: None,
            synthesised: true,
            span,
        })
    }

    fn function(&self, name: &str) -> Option<UFunction> {
        self.functions.get(name).cloned()
    }

    fn next_call_site(&mut self) -> usize {
        self.call_sites += 1;
        self.call_sites - 1
    }

    fn type_width(&self, name: &str) -> Option<u32> {
        self.types.get(name).copied()
    }

    fn package_scope(&self, package: &str) -> Option<Scope> {
        self.package_scopes.get(package).cloned()
    }

    fn declared_width_of(&self, name: &str) -> Option<u32> {
        self.lookup(name).map(|net| self.nets[net].width)
    }

    fn push_scope(&mut self) {
        Body::push_scope(self);
    }

    fn pop_scope(&mut self) {
        Body::pop_scope(self);
    }
}

impl Body {
    /// Adds a process, and records any hole inside its body against the module.
    ///
    /// A statement RTLScope could not model lives in the body as
    /// `StmtKind::Unsupported`, where a GUI counting `Module::skipped` would
    /// never see it. Copying it out here is what keeps the "N constructs
    /// skipped" badge honest about process bodies too (D2).
    fn push_process(&mut self, process: Process) {
        let mut holes = Vec::new();
        process.body.for_each_stmt(&mut |stmt| {
            if let rtlscope_ir::StmtKind::Unsupported { construct } = &stmt.kind {
                holes.push(Skipped { construct: construct.clone(), span: stmt.span });
            }
        });
        self.skipped.extend(holes);
        self.procs.push(process);
    }

    fn declare_net(&mut self, net: Net) -> NetId {
        let name = net.name.clone();
        let id = self.nets.alloc(net);
        if let Some(scope) = self.names.last_mut() {
            scope.insert(name, id);
        }
        id
    }

    /// Also make a net reachable by an unprefixed name, for references from
    /// inside the generate block that declared it.
    fn bind_name(&mut self, name: &str, id: NetId) {
        if let Some(scope) = self.names.last_mut() {
            scope.insert(name.to_string(), id);
        }
    }

    fn push_scope(&mut self) {
        self.names.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.names.pop();
    }

    fn lookup(&self, name: &str) -> Option<NetId> {
        self.names.iter().rev().find_map(|scope| scope.get(name).copied())
    }

    /// SystemVerilog creates a one-bit net for an undeclared name used in a
    /// connection. Real tools do it, so RTLScope does too — but says so, because
    /// it is far more often a typo than an intention.
    /// A name like `name`, `name#1`, ... that no net in scope has yet.
    fn unique_name(&self, name: &str) -> String {
        if self.lookup(name).is_none() {
            return name.to_string();
        }
        (1..).map(|n| format!("{name}#{n}")).find(|c| self.lookup(c).is_none()).unwrap_or_default()
    }

    fn lookup_or_create(&mut self, name: &str, span: Span, diags: &mut Diagnostics) -> NetId {
        if let Some(id) = self.lookup(name) {
            return id;
        }
        // `u_sub.count`: a name that reaches into an instance is a hierarchical
        // reference, which is not a wire and not in the subset. The stand-in
        // net keeps the rest of the process readable; the message says why
        // it is there.
        let into_instance = name
            .split_once('.')
            .is_some_and(|(head, _)| self.insts.iter().any(|inst| inst.name == head));
        if into_instance {
            diags.push(
                Diag::warning(
                    DiagCode::UnsupportedConstruct,
                    format!(
                        "`{name}` reaches into an instance; a hierarchical reference is outside \
                         the subset, so a one-bit net stands in for it"
                    ),
                )
                .at(span),
            );
        } else {
            diags.push(
                Diag::warning(
                    DiagCode::ImplicitNet,
                    format!("`{name}` was never declared; inferring a one-bit net"),
                )
                .at(span),
            );
        }
        self.declare_net(Net {
            name: name.to_string(),
            written: None,
            width: 1,
            kind: NetKind::Logic,
            type_name: None,
            synthesised: false,
            span,
        })
    }
}

/// What elaboration needs to know about a port of the module being instantiated.
struct ChildPort {
    name: String,
    dir: PortDir,
    /// The width the connection is coerced to, which is the width of the net a
    /// connection expression needs.
    width: u32,
}

/// Whether an expression names storage that could be driven from the far side.
///
/// A concatenation of signals can receive a value; `!rst_n` cannot.
fn is_lvalue(expr: &UExpr) -> bool {
    match &*expr.kind {
        UExprKind::Ident { .. } => true,
        UExprKind::Index { base, .. } | UExprKind::Range { base, .. } => is_lvalue(base),
        UExprKind::Concat { parts } => parts.iter().all(is_lvalue),
        _ => false,
    }
}

fn ident_name(expr: &UExpr) -> Option<String> {
    match &*expr.kind {
        UExprKind::Ident { name } => Some(name.clone()),
        _ => None,
    }
}

/// Collects every function declared anywhere in an item tree, generate blocks
/// included: a call inside a generate block still refers to the module's
/// functions, and a function declared inside one is visible to it.
/// `defs::WIDTH`: the spelling a qualified name is bound and looked up under.
fn qualified(package: &str, name: &str) -> String {
    format!("{package}::{name}")
}

fn bind(scope: &mut Scope, name: &str, value: i64, width: Option<u32>) {
    match width {
        Some(width) => scope.bind_sized(name, value, width),
        None => scope.bind(name, value),
    }
}

fn collect_functions(items: &[UItem], out: &mut HashMap<String, UFunction>) {
    for item in items {
        match item {
            UItem::Function { func } => {
                out.insert(func.name.clone(), func.clone());
            }
            UItem::GenerateFor { body, .. } => collect_functions(body, out),
            UItem::GenerateIf { then_items, else_items, .. } => {
                collect_functions(then_items, out);
                collect_functions(else_items, out);
            }
            UItem::GenerateBlock { items, .. } => collect_functions(items, out),
            _ => {}
        }
    }
}

/// Every module nothing else instantiates.
///
/// A design usually has exactly one, which is its top. Several means the file
/// set holds more than one design — the ordinary case for a project folder,
/// where a dozen blocks each have their own bench — and a caller has to say
/// which one is meant. Offered rather than only complained about, so a window
/// can turn them into buttons instead of asking somebody to retype one.
///
/// In declaration order, which is the order they were written in.
pub fn candidate_tops(uir: &UDesign) -> Vec<String> {
    let mut instantiated = HashSet::new();
    for module in &uir.modules {
        collect_instantiated(&module.items, &mut instantiated);
    }
    uir.modules
        .iter()
        .filter(|module| !instantiated.contains(module.name.as_str()))
        .map(|module| module.name.clone())
        .collect()
}

/// Collects every module name instantiated anywhere in an item tree.
fn collect_instantiated<'a>(items: &'a [UItem], out: &mut HashSet<&'a str>) {
    for item in items {
        match item {
            UItem::Inst { inst } => {
                out.insert(inst.module_name.as_str());
            }
            UItem::GenerateFor { body, .. } => collect_instantiated(body, out),
            UItem::GenerateIf { then_items, else_items, .. } => {
                collect_instantiated(then_items, out);
                collect_instantiated(else_items, out);
            }
            UItem::GenerateBlock { items, .. } => collect_instantiated(items, out),
            _ => {}
        }
    }
}

/// Gives every module the shortest name that still tells it apart.
///
/// A module elaborated once keeps its plain name, however many parameters it
/// has. Only when the same module was elaborated with *different* bindings does
/// it need a suffix, and then only the parameters that actually differ go in
/// it: `params_sub$W=16` and `params_sub$W=8`.
///
/// Measured on a real design, naming by every parameter produced a
/// 408-character label for a module that had no twin to be told apart from.
/// The full values are in `Module::params` either way, so nothing is lost.
fn name_specialisations(modules: &mut Arena<Module>) {
    // base name -> the parameter bindings each specialisation was built with
    let mut groups: HashMap<String, Vec<Vec<(String, i64)>>> = HashMap::new();
    for module in modules.iter() {
        groups.entry(module.base_name.clone()).or_default().push(bindings_of(module));
    }

    // Within a group, the parameters worth naming are the ones that vary.
    let distinguishing: HashMap<String, Vec<String>> = groups
        .iter()
        .map(|(base, all)| {
            if all.len() < 2 {
                return (base.clone(), Vec::new());
            }
            let first = &all[0];
            let names = first
                .iter()
                .filter(|(name, value)| {
                    all.iter().any(|other| other.iter().any(|(n, v)| n == name && v != value))
                })
                .map(|(name, _)| name.clone())
                .collect();
            (base.clone(), names)
        })
        .collect();

    for module in modules.iter_mut() {
        let Some(names) = distinguishing.get(&module.base_name) else { continue };
        if names.is_empty() {
            module.name = module.base_name.clone();
            continue;
        }
        let suffix: Vec<String> = module
            .params
            .iter()
            .filter(|param| names.contains(&param.name))
            .map(|param| format!("{}={}", param.name, param.value))
            .collect();
        module.name = if suffix.is_empty() {
            module.base_name.clone()
        } else {
            format!("{}${}", module.base_name, suffix.join(","))
        };
    }
}

/// The overridable parameter values a module was elaborated with.
fn bindings_of(module: &Module) -> Vec<(String, i64)> {
    module
        .params
        .iter()
        .filter(|param| !param.is_local)
        .map(|param| (param.name.clone(), param.value))
        .collect()
}

/// The name a module is built with, before [`name_specialisations`] shortens
/// it. Unique by construction, since the key is what made it a distinct module.
fn specialised_name(base: &str, key: &SpecKey) -> String {
    if key.1.is_empty() {
        return base.to_string();
    }
    let bindings: Vec<String> =
        key.1.iter().map(|(name, value)| format!("{name}={value}")).collect();
    format!("{base}${}", bindings.join(","))
}

fn code_for(error: &EvalError) -> DiagCode {
    match error {
        EvalError::UnknownName { .. } => DiagCode::ParamUnknown,
        EvalError::DivideByZero { .. } => DiagCode::DivisionByZero,
        _ => DiagCode::ParamNotConstant,
    }
}

/// Re-exported for the CLI, which reports connections by port name.
pub fn port_name(design: &Design, module: ModuleId, port: PortId) -> &str {
    &design.modules[module].ports[port.0 as usize].name
}
