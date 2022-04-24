//! This module is responsible for generating TypeScript type declaration files.
//! This code is run during the code generation phase along side the normal
//! Javascript code emission. Here we walk through the typed AST and translate
//! the Gleam statements into their TypeScript equivalent. Unlike the Javascript
//! code generation, the TypeScript generation only needs to look at the module
//! statements and not the expressions that may be _inside_ those statements.
//! This is due to the TypeScript declarations only caring about inputs and outputs
//! rather than _how_ those outputs are generated.
//!
//! ## Links
//! <https://www.typescriptlang.org/>
//! <https://www.typescriptlang.org/docs/handbook/declaration-files/introduction.html>

use std::{collections::HashMap, ops::Deref, sync::Arc};

use heck::ToUpperCamelCase;
use itertools::Itertools;

use crate::{
    ast::{
        Statement, TypedArg, TypedConstant, TypedExternalFnArg, TypedModule,
        TypedRecordConstructor, TypedStatement,
    },
    docvec,
    pretty::{break_, Document, Documentable},
    type_::{Type, TypeVar},
};

use super::{concat, import::Imports, line, lines, wrap_args, Output, INDENT};

/// The `TypePrinter` contains the logic of how to convert Gleam's type system
/// into the equivalent TypeScript type strings.
///
#[derive(Debug)]
struct TypePrinter<'a> {
    tracker: UsageTracker,
    current_module: &'a [String],
}

impl<'a> TypePrinter<'a> {
    fn new(current_module: &'a [String]) -> Self {
        Self {
            current_module,
            tracker: UsageTracker::default(),
        }
    }

    /// Converts a Gleam type into a TypeScript type string
    ///
    pub fn print(&mut self, type_: &Type) -> Document<'static> {
        self.do_print(type_, None)
    }

    /// Helper function for genering a TypeScript type string after collecting
    /// all of the generics used in a statement
    ///
    pub fn print_with_generic_usages(
        &mut self,
        type_: &Type,
        generic_usages: &HashMap<u64, u64>,
    ) -> Document<'static> {
        self.do_print(type_, Some(generic_usages))
    }

    fn do_print(
        &mut self,
        type_: &Type,
        generic_usages: Option<&HashMap<u64, u64>>,
    ) -> Document<'static> {
        match type_ {
            Type::Var { type_: typ } => self.print_var(&typ.borrow(), generic_usages),

            Type::App {
                name, module, args, ..
            } if module.is_empty() => self.print_prelude_type(name, args, generic_usages),

            Type::App {
                name, args, module, ..
            } => self.print_type_app(name, args, module, generic_usages),

            Type::Fn { args, retrn } => self.print_fn(args, retrn, generic_usages),

            Type::Tuple { elems } => tuple(elems.iter().map(|e| self.do_print(e, generic_usages))),
        }
    }

    fn print_var(
        &mut self,
        type_: &TypeVar,
        generic_usages: Option<&HashMap<u64, u64>>,
    ) -> Document<'static> {
        match type_ {
            TypeVar::Generic { id } => match &generic_usages {
                Some(usages) => match usages.get(id) {
                    Some(&0) => super::nil(),
                    Some(&1) => "any".to_doc(),
                    _ => id_to_type_var(*id),
                },
                None => id_to_type_var(*id),
            },
            // Shouldn't get here unless something went wrong
            TypeVar::Unbound { .. } => "any".to_doc(),
            TypeVar::Link { type_: typ } => self.do_print(typ, generic_usages),
        }
    }

    /// Prints a type coming from the Gleam prelude module. These are often the
    /// low level types the rest of the type system are built up from. If there
    /// is no built-in TypeScript equivalent, the type is prefixed with "$Gleam."
    /// and the Gleam prelude namespace will be imported during the code emission.
    ///
    fn print_prelude_type(
        &mut self,
        name: &str,
        args: &[Arc<Type>],
        generic_usages: Option<&HashMap<u64, u64>>,
    ) -> Document<'static> {
        match name {
            "Nil" => "null".to_doc(),
            "Int" | "Float" => "number".to_doc(),
            "UtfCodepoint" => {
                self.tracker.prelude_used = true;
                "$Gleam.UtfCodepoint".to_doc()
            }
            "String" => "string".to_doc(),
            "Bool" => "boolean".to_doc(),
            "BitString" => {
                self.tracker.prelude_used = true;
                "$Gleam.BitString".to_doc()
            }
            "List" => {
                self.tracker.prelude_used = true;
                docvec![
                    "$Gleam.List",
                    wrap_generic_args(args.iter().map(|x| self.do_print(x, generic_usages)))
                ]
            }
            "Result" => {
                self.tracker.prelude_used = true;
                docvec![
                    "$Gleam.Result",
                    wrap_generic_args(args.iter().map(|x| self.do_print(x, generic_usages)))
                ]
            }
            // Getting here sholud mean we either forgot a built-in type or there is a
            // compiler error
            name => panic!("{} is not a built-in type.", name),
        }
    }

    /// Prints a "named" programmer-defined Gleam type into the TypeScript
    /// equivalent.
    ///
    fn print_type_app(
        &mut self,
        name: &str,
        args: &[Arc<Type>],
        module: &[String],
        generic_usages: Option<&HashMap<u64, u64>>,
    ) -> Document<'static> {
        let name = format!("{}$", ts_safe_type_name(name.to_upper_camel_case()));
        let name = match module == self.current_module {
            true => Document::String(name),
            false => {
                // If type comes from a seperate module, use that module's nam
                // as a TypeScript namespace prefix
                docvec![
                    Document::String(format!("${}", module_name(module))),
                    ".",
                    Document::String(name),
                ]
            }
        };
        if args.is_empty() {
            return name;
        }

        // If the App type takes arguments, pass them in as TypeScript generics
        docvec![
            name,
            wrap_generic_args(args.iter().map(|a| self.do_print(a, generic_usages)))
        ]
    }

    /// Prints the TypeScript type for an anonymous function (aka lambda)
    ///
    fn print_fn(
        &mut self,
        args: &[Arc<Type>],
        retrn: &Type,
        generic_usages: Option<&HashMap<u64, u64>>,
    ) -> Document<'static> {
        docvec![
            wrap_args(args.iter().enumerate().map(|(idx, a)| docvec![
                "x",
                idx,
                ": ",
                self.do_print(a, generic_usages)
            ])),
            " => ",
            self.do_print(retrn, generic_usages)
        ]
    }

    /// Allows an outside module to mark the Gleam prelude as "used"
    ///
    pub fn set_prelude_used(&mut self) {
        self.tracker.prelude_used = true
    }

    /// Returns if the Gleam prelude has been used at all during the process
    /// of printing the TypeScript types
    ///
    pub fn prelude_used(&self) -> bool {
        self.tracker.prelude_used
    }
}

// When rendering a type variable to an TypeScript type spec we need all type
// variables with the same id to end up with the same name in the generated
// TypeScript. This function converts a usize into base 26 A-Z for this purpose.
fn id_to_type_var(id: u64) -> Document<'static> {
    if id < 26 {
        let mut name = "".to_string();
        name.push(std::char::from_u32((id % 26 + 65) as u32).expect("id_to_type_var 0"));
        return Document::String(name);
    }
    let mut name = vec![];
    let mut last_char = id;
    while last_char >= 26 {
        name.push(std::char::from_u32((last_char % 26 + 65) as u32).expect("id_to_type_var 1"));
        last_char /= 26;
    }
    name.push(std::char::from_u32((last_char % 26 + 64) as u32).expect("id_to_type_var 2"));
    name.reverse();
    Document::String(name.into_iter().collect())
}

fn name_with_generics<'a>(
    name: Document<'a>,
    types: impl IntoIterator<Item = &'a Arc<Type>>,
) -> Document<'a> {
    let generic_usages = collect_generic_usages(HashMap::new(), types);
    let generic_names: Vec<Document<'_>> = generic_usages
        .iter()
        .map(|(id, _use_count)| id_to_type_var(*id))
        .collect();

    docvec![
        name,
        if generic_names.is_empty() {
            super::nil()
        } else {
            wrap_generic_args(generic_names)
        },
    ]
}

// A generic can either be rendered as an actual type variable such as `A` or `B`,
// or it can be rendered as `any` depending on how many usages it has. If it
// has only 1 usage it is an `any` type. If it has more than 1 usage it is a
// TS generic. This function gathers usages for this determination.
//
//   Examples:
//     fn(a) -> String       // `a` is `any`
//     fn() -> Result(a, b)  // `a` and `b` are `any`
//     fn(a) -> a            // `a` is a generic
fn collect_generic_usages<'a>(
    mut ids: HashMap<u64, u64>,
    types: impl IntoIterator<Item = &'a Arc<Type>>,
) -> HashMap<u64, u64> {
    for typ in types {
        generic_ids(typ, &mut ids);
    }
    ids
}

fn generic_ids(type_: &Type, ids: &mut HashMap<u64, u64>) {
    match type_ {
        Type::Var { type_: typ } => match typ.borrow().deref() {
            TypeVar::Generic { id, .. } => {
                let count = ids.entry(*id).or_insert(0);
                *count += 1;
            }
            TypeVar::Unbound { .. } => (),
            TypeVar::Link { type_: typ } => generic_ids(typ, ids),
        },
        Type::App { args, .. } => {
            for arg in args {
                generic_ids(arg, ids)
            }
        }
        Type::Fn { args, retrn } => {
            for arg in args {
                generic_ids(arg, ids)
            }
            generic_ids(retrn, ids);
        }
        Type::Tuple { elems } => {
            for elem in elems {
                generic_ids(elem, ids)
            }
        }
    }
}

/// Prints a Gleam tuple in the TypeScript equivalent syntax
///
fn tuple<'a>(elems: impl IntoIterator<Item = Document<'a>>) -> Document<'a> {
    break_("", "")
        .append(concat(Itertools::intersperse(
            elems.into_iter(),
            break_(",", ", "),
        )))
        .nest(INDENT)
        .append(break_("", ""))
        .surround("[", "]")
        .group()
}

fn wrap_generic_args<'a, I>(args: I) -> Document<'a>
where
    I: IntoIterator<Item = Document<'a>>,
{
    break_("", "")
        .append(concat(Itertools::intersperse(
            args.into_iter(),
            break_(",", ", "),
        )))
        .nest(INDENT)
        .append(break_("", ""))
        .surround("<", ">")
        .group()
}

/// Returns a name that can be used as a TypeScript type name. If there is a
/// naming clash a '_' will be appended.
///
fn ts_safe_type_name(mut name: String) -> String {
    if matches!(
        name.as_str(),
        "any"
            | "boolean"
            | "constructor"
            | "declare"
            | "get"
            | "module"
            | "require"
            | "number"
            | "set"
            | "string"
            | "symbol"
            | "type"
            | "from"
            | "of"
    ) {
        name.push('_');
        name
    } else {
        super::maybe_escape_identifier_string(&name)
    }
}

/// The `TypeScriptGenerator` contains the logic of how to convert Gleam's typed
/// AST into the equivalent TypeScript type declaration file.
///
#[derive(Debug)]
pub struct TypeScriptGenerator<'a> {
    module: &'a TypedModule,
    type_printer: TypePrinter<'a>,
}

/// Joins the parts of the import into a single `UpperCamelCase` string
///
fn module_name(parts: &[String]) -> String {
    parts.iter().map(|x| x.to_upper_camel_case()).join("")
}

impl<'a> TypeScriptGenerator<'a> {
    pub fn new(module: &'a TypedModule) -> Self {
        Self {
            module,
            type_printer: TypePrinter::new(&module.name),
        }
    }

    pub fn compile(&mut self) -> Output<'a> {
        let mut imports = self.collect_imports();
        let statements = self
            .module
            .statements
            .iter()
            .flat_map(|s| self.statement(s));

        // Two lines between each statement
        let mut statements: Vec<_> =
            Itertools::intersperse(statements, Ok(lines(2))).try_collect()?;

        // Put it all together

        if self.type_printer.prelude_used() {
            let path = self.import_path(&self.module.type_info.package, &["gleam".to_string()]);
            imports.register_module(path, ["$Gleam".to_string()], []);
        }

        if imports.is_empty() && statements.is_empty() {
            Ok(docvec!("export {}", line()))
        } else if imports.is_empty() {
            statements.push(line());
            Ok(statements.to_doc())
        } else if statements.is_empty() {
            Ok(imports.into_doc())
        } else {
            Ok(docvec![imports.into_doc(), line(), statements, line()])
        }
    }

    fn collect_imports(&mut self) -> Imports<'a> {
        let mut imports = Imports::new();

        for statement in &self.module.statements {
            match statement {
                Statement::Fn { .. }
                | Statement::TypeAlias { .. }
                | Statement::CustomType { .. }
                | Statement::ExternalType { .. }
                | Statement::ExternalFn { .. }
                | Statement::ModuleConstant { .. } => (),

                Statement::Import {
                    module, package, ..
                } => {
                    self.register_import(&mut imports, package, module);
                }
            }
        }

        imports
    }

    /// Registers an import of an external module so that it can be added to
    /// the top of the generated Document. The module names are prefixed with a
    /// "$" symbol to prevent any clashes with other Gleam names that may be
    /// used in this module.
    ///
    fn register_import(
        &mut self,
        imports: &mut Imports<'a>,
        package: &'a str,
        module: &'a [String],
    ) {
        let path = self.import_path(package, module);
        imports.register_module(path, [format!("${}", module_name(module))], []);
    }

    /// Calculates the path of where to import an external module from
    ///
    fn import_path(&self, package: &'a str, module: &'a [String]) -> String {
        let path = module.join("/");

        // TODO: strip shared prefixed between current module and imported
        // module to avoid decending and climbing back out again
        if package == self.module.type_info.package || package.is_empty() {
            // Same package
            match self.module.name.len() {
                1 => format!("./{}.d.ts", path),
                _ => {
                    let prefix = "../".repeat(self.module.name.len() - 1);
                    format!("{}{}.d.ts", prefix, path)
                }
            }
        } else {
            // Different package
            let prefix = "../".repeat(self.module.name.len() + 1);
            format!("{}{}/dist/{}.d.ts", prefix, package, path)
        }
    }

    fn statement(&mut self, statement: &'a TypedStatement) -> Vec<Output<'a>> {
        match statement {
            Statement::TypeAlias {
                alias,
                public,
                type_,
                ..
            } if *public => vec![self.type_alias(alias, type_)],
            Statement::TypeAlias { .. } => vec![],

            Statement::ExternalType {
                public,
                name,
                arguments,
                ..
            } if *public => vec![self.external_type(name, arguments)],
            Statement::ExternalType { .. } => vec![],

            Statement::Import { .. } => vec![],

            Statement::CustomType {
                public,
                constructors,
                opaque,
                name,
                typed_parameters,
                ..
            } if *public => {
                self.custom_type_definition(name, typed_parameters, constructors, *opaque)
            }
            Statement::CustomType { .. } => vec![],

            Statement::ModuleConstant {
                public,
                name,
                value,
                ..
            } if *public => vec![self.module_constant(name, value)],
            Statement::ModuleConstant { .. } => vec![],

            Statement::Fn {
                arguments,
                name,
                public,
                return_type,
                ..
            } if *public => vec![self.module_function(name, arguments, return_type)],
            Statement::Fn { .. } => vec![],

            Statement::ExternalFn {
                public,
                name,
                arguments,
                return_type,
                ..
            } if *public => vec![self.external_function(name, arguments, return_type)],
            Statement::ExternalFn { .. } => vec![],
        }
    }

    fn external_type(&self, name: &str, args: &'a [String]) -> Output<'a> {
        let doc_name = Document::String(format!("{}$", ts_safe_type_name(name.to_string())));
        if args.is_empty() {
            Ok(docvec!["export type ", doc_name, " = any;"])
        } else {
            Ok(docvec![
                "export type ",
                doc_name,
                wrap_generic_args(
                    args.iter()
                        .map(|x| Document::String(x.to_upper_camel_case()))
                ),
                " = any;",
            ])
        }
    }

    fn type_alias(&mut self, alias: &str, type_: &Type) -> Output<'a> {
        Ok(docvec![
            "export type ",
            Document::String(ts_safe_type_name(alias.to_string())),
            " = ",
            self.type_printer.print(type_),
            ";"
        ])
    }

    /// Converts a Gleam custom type definition into the TypeScript equivalent.
    /// In Gleam, all custom types have one to many concrete constructors. This
    /// function first converts the constructors into TypeScript then finally
    /// emits a union type to represent the TypeScript type itself. Because in
    /// Gleam constructors can have the same name as the custom type, here we
    /// append a "$" symbol to the emited TypeScript type to prevent those
    /// naming clases.
    ///
    fn custom_type_definition(
        &mut self,
        name: &'a str,
        typed_parameters: &'a [Arc<Type>],
        constructors: &'a [TypedRecordConstructor],
        opaque: bool,
    ) -> Vec<Output<'a>> {
        let mut definitions: Vec<Output<'_>> = constructors
            .iter()
            .map(|constructor| Ok(self.record_definition(constructor, opaque)))
            .collect();

        definitions.push(Ok(docvec![
            "export type ",
            name_with_generics(Document::String(format!("{}$", name)), typed_parameters),
            " = ",
            concat(Itertools::intersperse(
                constructors.iter().map(|x| name_with_generics(
                    super::maybe_escape_identifier_doc(&x.name),
                    x.arguments.iter().map(|a| &a.type_)
                )),
                break_("| ", " | "),
            )),
            ";",
        ]));

        definitions
    }

    fn record_definition(
        &mut self,
        constructor: &'a TypedRecordConstructor,
        opaque: bool,
    ) -> Document<'a> {
        self.type_printer.set_prelude_used();
        let head = docvec![
            // opaque type constructors are not exposed to JS
            if opaque {
                super::nil()
            } else {
                "export ".to_doc()
            },
            "class ",
            name_with_generics(
                super::maybe_escape_identifier_doc(&constructor.name),
                constructor.arguments.iter().map(|a| &a.type_)
            ),
            " extends $Gleam.CustomType {"
        ];

        if constructor.arguments.is_empty() {
            return head.append("}");
        };

        let class_body = docvec![
            line(),
            // First add the constructor
            "constructor",
            wrap_args(constructor.arguments.iter().enumerate().map(|(i, arg)| {
                let name = arg
                    .label
                    .as_ref()
                    .map(|s| super::maybe_escape_identifier_doc(s))
                    .unwrap_or_else(|| Document::String(format!("{}", i)));
                docvec![name, ": ", self.type_printer.print(&arg.type_)]
            })),
            ";",
            line(),
            line(),
            // Then add each field to the class
            concat(Itertools::intersperse(
                constructor.arguments.iter().enumerate().map(|(i, arg)| {
                    let name = arg
                        .label
                        .as_ref()
                        .map(|s| super::maybe_escape_identifier_doc(s))
                        .unwrap_or_else(|| Document::String(format!("x{}", i)));
                    docvec![name, ": ", self.type_printer.print(&arg.type_), ";"]
                }),
                line(),
            )),
        ]
        .nest(INDENT);

        docvec![head, class_body, line(), "}"]
    }

    fn module_constant(&mut self, name: &'a str, value: &'a TypedConstant) -> Output<'a> {
        Ok(docvec![
            "export const ",
            super::maybe_escape_identifier_doc(name),
            ": ",
            self.type_printer.print(&value.type_()),
            ";",
        ])
    }

    fn module_function(
        &mut self,
        name: &'a str,
        args: &'a [TypedArg],
        return_type: &'a Arc<Type>,
    ) -> Output<'a> {
        let generic_usages = collect_generic_usages(
            HashMap::new(),
            std::iter::once(return_type).chain(args.iter().map(|a| &a.type_)),
        );
        let generic_names: Vec<Document<'_>> = generic_usages
            .iter()
            .filter(|(_id, use_count)| **use_count > 1)
            .map(|(id, _use_count)| id_to_type_var(*id))
            .collect();

        Ok(docvec![
            "export function ",
            super::maybe_escape_identifier_doc(name),
            if generic_names.is_empty() {
                super::nil()
            } else {
                wrap_generic_args(generic_names)
            },
            wrap_args(
                args.iter()
                    .enumerate()
                    .map(|(i, a)| match a.get_variable_name() {
                        None => {
                            docvec![
                                "x",
                                i,
                                ": ",
                                self.type_printer
                                    .print_with_generic_usages(&a.type_, &generic_usages)
                            ]
                        }
                        Some(name) => docvec![
                            super::maybe_escape_identifier_doc(name),
                            ": ",
                            self.type_printer
                                .print_with_generic_usages(&a.type_, &generic_usages)
                        ],
                    }),
            ),
            ": ",
            self.type_printer
                .print_with_generic_usages(return_type, &generic_usages),
            ";",
        ])
    }

    fn external_function(
        &mut self,
        name: &'a str,
        args: &'a [TypedExternalFnArg],
        return_type: &'a Arc<Type>,
    ) -> Output<'a> {
        let generic_usages = collect_generic_usages(
            HashMap::new(),
            std::iter::once(return_type).chain(args.iter().map(|a| &a.type_)),
        );
        let generic_names: Vec<Document<'_>> = generic_usages
            .iter()
            .filter(|(_id, use_count)| **use_count > 1)
            .map(|(id, _use_count)| id_to_type_var(*id))
            .collect();

        Ok(docvec![
            "export function ",
            super::maybe_escape_identifier_doc(name),
            if generic_names.is_empty() {
                super::nil()
            } else {
                wrap_generic_args(generic_names)
            },
            wrap_args(args.iter().enumerate().map(|(i, a)| match &a.label {
                None => {
                    docvec![
                        "x",
                        i,
                        ": ",
                        self.type_printer
                            .print_with_generic_usages(&a.type_, &generic_usages)
                    ]
                }
                Some(name) => docvec![
                    super::maybe_escape_identifier_doc(name),
                    ": ",
                    self.type_printer.print_with_generic_usages(&a.type_, &generic_usages)
                ],
            })),
            ": ",
            self.type_printer
                .print_with_generic_usages(return_type, &generic_usages),
            ";",
        ])
    }
}

#[derive(Debug, Default)]
pub(crate) struct UsageTracker {
    pub prelude_used: bool,
}