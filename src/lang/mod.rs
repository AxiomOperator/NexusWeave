//! Grammar registry.

use crate::model::Lang;
use tree_sitter::Language;

pub fn grammar(lang: Lang) -> Language {
    match lang {
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        // The TSX grammar is a superset of JSX + Flow-free JS; parsing JS with it keeps
        // one extractor and handles type-annotated `.js` written by mistake.
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Java => tree_sitter_java::LANGUAGE.into(),
        Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        Lang::C => tree_sitter_c::LANGUAGE.into(),
        Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
    }
}

thread_local! {
    static PARSERS: std::cell::RefCell<Vec<Option<tree_sitter::Parser>>> =
        std::cell::RefCell::new((0..Lang::ALL.len()).map(|_| None).collect());
}

/// Parse with a per-thread cached parser.
pub fn parse(lang: Lang, src: &[u8]) -> Option<tree_sitter::Tree> {
    PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        let slot = &mut parsers[lang as usize];
        if slot.is_none() {
            let mut p = tree_sitter::Parser::new();
            p.set_language(&grammar(lang)).ok()?;
            *slot = Some(p);
        }
        let p = slot.as_mut().unwrap();
        p.reset();
        p.parse(src, None)
    })
}
