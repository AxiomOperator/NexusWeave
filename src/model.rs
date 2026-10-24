//! Core data model shared by extraction, storage, resolution and queries.

use serde::Serialize;

macro_rules! int_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $val:expr => $s:expr),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u8)]
        pub enum $name { $($variant = $val),* }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),*];
            pub fn as_str(self) -> &'static str { match self { $($name::$variant => $s),* } }
            pub fn from_i64(v: i64) -> Option<Self> { match v { $($val => Some($name::$variant),)* _ => None } }
            pub fn parse(s: &str) -> Option<Self> {
                let s = s.to_ascii_lowercase();
                $(if s == $s { return Some($name::$variant); })*
                None
            }
        }
        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { s.serialize_str(self.as_str()) }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

int_enum!(NodeKind {
    File = 0 => "file",
    Module = 1 => "module",
    Namespace = 2 => "namespace",
    Class = 3 => "class",
    Struct = 4 => "struct",
    Interface = 5 => "interface",
    Trait = 6 => "trait",
    Enum = 7 => "enum",
    EnumMember = 8 => "enum_member",
    Function = 9 => "function",
    Method = 10 => "method",
    Constructor = 11 => "constructor",
    Property = 12 => "property",
    Field = 13 => "field",
    Variable = 14 => "variable",
    Constant = 15 => "constant",
    TypeAlias = 16 => "type_alias",
    Macro = 17 => "macro",
    Union = 18 => "union",
    // HTTP route registration (`GET /users/{id}`), linked to its handler.
    Route = 19 => "route",
    // event topic registered by on("name", h) / addEventListener / `Evt += h`; listeners hang off it
    Event = 20 => "event",
});

impl NodeKind {
    /// Kinds that can own members and participate in inheritance.
    pub fn is_type(self) -> bool {
        matches!(self, NodeKind::Class | NodeKind::Struct | NodeKind::Interface | NodeKind::Trait
            | NodeKind::Enum | NodeKind::TypeAlias | NodeKind::Union)
    }
    pub fn is_callable(self) -> bool {
        matches!(self, NodeKind::Function | NodeKind::Method | NodeKind::Constructor | NodeKind::Macro)
    }
    pub fn is_container(self) -> bool {
        self.is_type() || matches!(self, NodeKind::Module | NodeKind::Namespace)
    }
}

int_enum!(
    /// Relationship kinds. Everything except `Contains` is stored as a resolved reference.
    RefKind {
    Call = 0 => "calls",
    Instantiate = 1 => "instantiates",
    Extends = 2 => "extends",
    Implements = 3 => "implements",
    TypeRef = 4 => "references",
    Import = 5 => "imports",
    Read = 6 => "reads",
    // a function/method passed as a value (callback, handler, event listener)
    Callback = 7 => "passes",
    // method overrides / implements a supertype method (synthesized by the resolver)
    Overrides = 8 => "overrides",
    // traversal-only: interface/base method → an implementation (reverse of `overrides`); never stored
    Dispatch = 9 => "dispatches",
    // route → its handler (synthesized from a decorator / registration, no call site)
    Handles = 10 => "handles",
    // raw emit site: emit("name") (never resolved itself; the resolver synthesizes `triggers`)
    Emit = 11 => "emits",
    // emit site → every event node with that name (synthesized)
    Triggers = 12 => "triggers",
    // assignment to a property: runs its setter (`self.debug = x`); only properties are targets
    Write = 13 => "writes",
});

int_enum!(Lang {
    TypeScript = 0 => "typescript",
    Tsx = 1 => "tsx",
    JavaScript = 2 => "javascript",
    Python = 3 => "python",
    Go = 4 => "go",
    Rust = 5 => "rust",
    Java = 6 => "java",
    CSharp = 7 => "csharp",
    C = 8 => "c",
    Cpp = 9 => "cpp",
    Ruby = 10 => "ruby",
    Php = 11 => "php",
    Kotlin = 12 => "kotlin",
    Swift = 13 => "swift",
    Dart = 14 => "dart",
    Scala = 15 => "scala",
    Lua = 16 => "lua",
    // single-file components: the <script> block is TS/JS
    Vue = 17 => "vue",
    Svelte = 18 => "svelte",
});

impl Lang {
    pub fn from_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit_once('.').map(|(_, e)| e)?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "rs" => Lang::Rust,
            "java" => Lang::Java,
            "cs" => Lang::CSharp,
            "c" => Lang::C,
            "h" => Lang::Cpp.header_lang(path),
            "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ipp" | "inl" => Lang::Cpp,
            "rb" | "rake" => Lang::Ruby,
            "php" | "phtml" => Lang::Php,
            "kt" | "kts" => Lang::Kotlin,
            "swift" => Lang::Swift,
            "dart" => Lang::Dart,
            "scala" | "sc" => Lang::Scala,
            "lua" => Lang::Lua,
            "vue" => Lang::Vue,
            "svelte" => Lang::Svelte,
            _ => return None,
        })
    }
    /// `.h` is ambiguous; the C++ grammar is a superset that parses C headers fine.
    fn header_lang(self, _path: &str) -> Lang { Lang::Cpp }

    /// Languages that share one symbol/import namespace for resolution purposes.
    pub fn family(self) -> u8 {
        match self {
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Vue | Lang::Svelte => 0,
            Lang::C | Lang::Cpp => 1,
            other => other as u8 + 10,
        }
    }
}

/// A symbol extracted from one file. `parent` indexes into the same file's node list.
#[derive(Debug, Clone)]
pub struct ExNode {
    pub kind: NodeKind,
    pub name: String,
    pub qname: String,
    pub parent: Option<u32>,
    /// Owner type name for members declared out-of-line (Go receivers, Rust impls, C++ `A::f`).
    pub owner: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: u32,
    pub end_byte: u32,
    pub exported: bool,
    pub signature: Option<String>,
    pub doc: Option<String>,
    /// Declared type of a field/variable/property, or return type of a callable.
    pub type_hint: Option<String>,
    /// Receiver type of an extension method (C# `this T x`).
    pub ext_of: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExRef {
    /// Index of the enclosing node in the file's node list (0 = the file itself).
    pub from: u32,
    pub kind: RefKind,
    pub name: String,
    /// Textual receiver / namespace, e.g. `fmt` in `fmt.Println`, `this.repo` in `this.repo.save()`.
    pub qualifier: Option<String>,
    /// Receiver type inferred locally during extraction.
    pub recv_type: Option<String>,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ImportKind {
    /// `import { a as b } from "m"` / `from m import a as b` / `use m::a as b`
    Named = 0,
    /// `import * as ns from "m"` / `import m` / Go `import alias "m"` — binds the module itself.
    Namespace = 1,
    /// `from m import *` / `using Ns;` / Java `import a.b.*` / `#include` — brings everything in scope.
    Glob = 2,
    /// `export { a } from "m"` / `pub use` — re-export.
    ReExport = 3,
    /// `export * from "m"`
    ReExportAll = 4,
    /// `import "m"` / `require_relative` — side-effect file dependency only.
    SideEffect = 5,
}

impl ImportKind {
    pub fn from_i64(v: i64) -> ImportKind {
        match v {
            0 => ImportKind::Named,
            1 => ImportKind::Namespace,
            2 => ImportKind::Glob,
            3 => ImportKind::ReExport,
            4 => ImportKind::ReExportAll,
            _ => ImportKind::SideEffect,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExImport {
    pub kind: ImportKind,
    pub module: String,
    /// Name in the source module (empty for namespace/glob/side-effect).
    pub imported: String,
    /// Name bound locally.
    pub local: String,
    pub line: u32,
}

/// A router mounted under a path prefix: `app.use("/api", router)`, `register_blueprint(bp, url_prefix=..)`.
#[derive(Debug, Clone)]
pub struct ExMount {
    /// Binding (as written) of the router/app the child is mounted on.
    pub parent: String,
    /// Binding (as written) of the mounted router.
    pub child: String,
    pub prefix: String,
    pub line: u32,
}

#[derive(Debug, Default, Clone)]
pub struct FileExtraction {
    pub nodes: Vec<ExNode>,
    pub refs: Vec<ExRef>,
    pub imports: Vec<ExImport>,
    pub mounts: Vec<ExMount>,
    /// Package / namespace declared by the file (Go package, Java package, C# / PHP namespace).
    pub package: Option<String>,
    pub parse_errors: u32,
}
