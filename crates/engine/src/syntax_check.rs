//! Post-fix syntax validation using OXC parser.
//!
//! After the LLM writes a file to disk, this module parses it to detect
//! syntax errors introduced by the fix. LLMs routinely normalize Unicode
//! punctuation (curly quotes → ASCII apostrophes), break string delimiters,
//! or produce malformed JSX — catching these immediately allows the engine
//! to restore the original and retry or mark the file as failed.

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::path::Path;

/// File extensions that should be syntax-checked.
const CHECKABLE_EXTENSIONS: &[&str] = &["js", "jsx", "ts", "tsx", "mjs", "mts"];

/// Determine the OXC `SourceType` from a file path and source content.
///
/// Public so that `goose_client::validate_post_fix` can parse the
/// pre-fix snapshot without writing to a temporary file.
///
/// Matches the logic used in `frontend-analyzer-provider/crates/js-scanner`.
pub fn source_type_for_path(path: &Path, source: &str) -> SourceType {
    let ext = path.extension().unwrap_or_default().to_string_lossy();

    let base = match ext.as_ref() {
        "tsx" => return SourceType::tsx(),
        "ts" | "mts" => return SourceType::ts(),
        "jsx" => return SourceType::jsx(),
        "cjs" => return SourceType::cjs().with_jsx(true),
        "mjs" => return SourceType::mjs().with_jsx(true),
        "js" => {
            let has_import = source.contains("import ")
                && (source.contains(" from ") || source.contains("import {"));
            let has_require =
                source.contains("require(") || source.contains("module.exports");

            if has_import {
                SourceType::mjs()
            } else if has_require {
                SourceType::cjs()
            } else {
                SourceType::mjs()
            }
        }
        _ => SourceType::mjs(),
    };

    base.with_jsx(true)
}

/// Check whether a file extension is one we can syntax-check.
pub fn is_checkable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| CHECKABLE_EXTENSIONS.contains(&ext))
}

/// Parse a JS/TS/JSX/TSX file and return any syntax errors.
///
/// Returns `Ok(vec![])` when the file parses without errors, or
/// `Ok(vec![...])` with human-readable error messages when syntax
/// errors are found. Returns `Err` only on I/O failures.
///
/// Files with non-JS/TS extensions are silently skipped (returns empty vec).
pub fn check_syntax(file_path: &Path) -> std::io::Result<Vec<String>> {
    if !is_checkable(file_path) {
        return Ok(Vec::new());
    }

    let source = std::fs::read_to_string(file_path)?;
    let source_type = source_type_for_path(file_path, &source);

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source, source_type).parse();

    let mut errors = Vec::new();

    if ret.panicked {
        let msg = ret
            .errors
            .first()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "parser panicked (unknown error)".to_string());
        errors.push(msg);
    } else if !ret.errors.is_empty() {
        for err in &ret.errors {
            errors.push(err.to_string());
        }
    }

    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn valid_tsx_passes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Component.tsx");
        std::fs::write(&file, "export const App = () => <div>Hello</div>;\n").unwrap();
        let errors = check_syntax(&file).unwrap();
        assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
    }

    #[test]
    fn broken_string_detected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Bad.tsx");
        // Unescaped apostrophe inside single-quoted string
        std::fs::write(&file, "const x = 'Let's go';\n").unwrap();
        let errors = check_syntax(&file).unwrap();
        assert!(!errors.is_empty(), "expected syntax errors for broken string");
    }

    #[test]
    fn non_js_file_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("package.json");
        std::fs::write(&file, "{ invalid json !!!\n").unwrap();
        let errors = check_syntax(&file).unwrap();
        assert!(errors.is_empty(), "non-JS files should be skipped");
    }

    #[test]
    fn valid_ts_passes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("util.ts");
        std::fs::write(
            &file,
            "export function greet(name: string): string { return `Hello ${name}`; }\n",
        )
        .unwrap();
        let errors = check_syntax(&file).unwrap();
        assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
    }

    #[test]
    fn unterminated_template_detected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Bad.ts");
        let mut f = std::fs::File::create(&file).unwrap();
        // Unterminated template literal
        writeln!(f, "const x = `hello ${{world").unwrap();
        let errors = check_syntax(&file).unwrap();
        assert!(
            !errors.is_empty(),
            "expected syntax errors for unterminated template"
        );
    }

    #[test]
    fn is_checkable_works() {
        assert!(is_checkable(Path::new("foo.tsx")));
        assert!(is_checkable(Path::new("bar.ts")));
        assert!(is_checkable(Path::new("baz.jsx")));
        assert!(is_checkable(Path::new("qux.js")));
        assert!(is_checkable(Path::new("m.mjs")));
        assert!(is_checkable(Path::new("m.mts")));
        assert!(!is_checkable(Path::new("package.json")));
        assert!(!is_checkable(Path::new("style.css")));
        assert!(!is_checkable(Path::new("README.md")));
    }
}
