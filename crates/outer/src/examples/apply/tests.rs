//! Behaviour tests for the apply engine: the contract's formatting rules.
//!
//! `tests/fixtures/apply/` is a verbatim copy of outer-templates'
//! `scripts/apply-fixtures/`, the cases the TypeScript engine is tested on;
//! [`shared_golden_cases_match_the_typescript_engine`] runs them here.

use super::*;
use std::collections::BTreeMap;

fn tree(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect()
}

fn example(name: &str, template: &str, files: &[(&str, &str)]) -> ExampleInput {
    ExampleInput {
        name: name.to_string(),
        template: template.to_string(),
        requires: Vec::new(),
        nav: Vec::new(),
        bindings: BTreeMap::new(),
        files: files
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect(),
    }
}

const NAV_TS: &str = "import { House, Settings } from \"lucide\"; // outer:nav-icons\nimport type { LucideIcon } from \"lucide\";\n\nexport const NAV: readonly NavItem[] = [\n  { href: \"/\", label: \"Home\", icon: House },\n  // outer:nav-items — add_example inserts pages above this line\n  { href: \"/settings\", label: \"Settings\", icon: Settings },\n];\n";
const VOID_CONFIG: &str = "export default {\n  cloudflare: {\n    // outer:bindings — add_example inserts bindings above this line\n  },\n};\n";
const PACKAGE_JSON: &str = "{\n  \"name\": \"seed\",\n  \"private\": true\n}\n";

fn app_tree() -> BTreeMap<String, String> {
    tree(&[
        ("pages/_nav.ts", NAV_TS),
        ("void.config.ts", VOID_CONFIG),
        ("package.json", PACKAGE_JSON),
    ])
}

#[test]
fn copies_files_and_tracks_added() {
    let tree = app_tree();
    let example = example(
        "list-form",
        "app",
        &[("pages/list.tsx", "export default 1;\n")],
    );
    let plan = plan_apply(&tree, "app", &[example], &["list-form"]).unwrap();
    assert_eq!(plan.added, vec!["list-form".to_string()]);
    assert_eq!(plan.files, vec!["pages/list.tsx".to_string()]);
    assert!(plan.nav.is_empty());
    assert_eq!(
        plan.writes,
        vec![(
            "pages/list.tsx".to_string(),
            "export default 1;\n".to_string()
        )]
    );
}

#[test]
fn wrong_template_refuses() {
    let tree = app_tree();
    let example = example("blog", "website", &[("src/x.astro", "x\n")]);
    let err = plan_apply(&tree, "app", &[example], &["blog"]).unwrap_err();
    assert!(err.0.contains("website"), "unexpected: {}", err.0);
}

#[test]
fn conflicting_file_refuses_with_path() {
    let mut tree = app_tree();
    tree.insert("pages/list.tsx".to_string(), "different\n".to_string());
    let example = example(
        "list-form",
        "app",
        &[("pages/list.tsx", "export default 1;\n")],
    );
    let err = plan_apply(&tree, "app", &[example], &["list-form"]).unwrap_err();
    assert!(err.0.contains("pages/list.tsx"), "unexpected: {}", err.0);
}

#[test]
fn identical_files_count_as_applied() {
    let mut tree = app_tree();
    tree.insert(
        "pages/list.tsx".to_string(),
        "export default 1;\n".to_string(),
    );
    let example = example(
        "list-form",
        "app",
        &[("pages/list.tsx", "export default 1;\n")],
    );
    let err = plan_apply(&tree, "app", &[example], &["list-form"]).unwrap_err();
    assert!(err.0.contains("already applied"), "unexpected: {}", err.0);
}

#[test]
fn requires_resolve_depth_first_and_deduped() {
    let tree = app_tree();
    let passkeys = example("passkeys", "app", &[("pages/sign-in.tsx", "s\n")]);
    let mut photos = example("photos", "app", &[("pages/photos.tsx", "p\n")]);
    photos.requires = vec!["passkeys".to_string()];
    let plan = plan_apply(&tree, "app", &[passkeys, photos], &["photos"]).unwrap();
    assert_eq!(
        plan.added,
        vec!["passkeys".to_string(), "photos".to_string()]
    );
    // Requesting both keeps one copy, requires first.
    let passkeys = example("passkeys", "app", &[("pages/sign-in.tsx", "s\n")]);
    let mut photos = example("photos", "app", &[("pages/photos.tsx", "p\n")]);
    photos.requires = vec!["passkeys".to_string()];
    let plan = plan_apply(&tree, "app", &[passkeys, photos], &["photos", "passkeys"]).unwrap();
    assert_eq!(
        plan.added,
        vec!["passkeys".to_string(), "photos".to_string()]
    );
}

#[test]
fn requires_cycle_refuses() {
    let tree = app_tree();
    let mut one = example("one", "app", &[("a.txt", "a\n")]);
    let mut two = example("two", "app", &[("b.txt", "b\n")]);
    one.requires = vec!["two".to_string()];
    two.requires = vec!["one".to_string()];
    let err = plan_apply(&tree, "app", &[one, two], &["one"]).unwrap_err();
    assert!(err.0.contains("cycle"), "unexpected: {}", err.0);
}

#[test]
fn requires_chain_past_depth_3_refuses() {
    let tree = app_tree();
    let mk = |name: &str, dep: Option<&str>| {
        let mut example = example(name, "app", &[(name, "x\n")]);
        example.requires = dep.map(|dep| vec![dep.to_string()]).unwrap_or_default();
        example
    };
    let examples = vec![
        mk("e0", Some("e1")),
        mk("e1", Some("e2")),
        mk("e2", Some("e3")),
        mk("e3", Some("e4")),
        mk("e4", None),
    ];
    let err = plan_apply(&tree, "app", &examples, &["e0"]).unwrap_err();
    assert!(err.0.contains("deeper than 3"), "unexpected: {}", err.0);
}

#[test]
fn unknown_example_names_valid_choice() {
    let tree = app_tree();
    let example = example("passkeys", "app", &[("a.txt", "a\n")]);
    let err = plan_apply(&tree, "app", &[example], &["nope"]).unwrap_err();
    assert!(err.0.contains("passkeys"), "unexpected: {}", err.0);
}

#[test]
fn migrations_renumber_after_highest() {
    let mut tree = app_tree();
    tree.insert(
        "db/migrations/0002_first.sql".to_string(),
        "CREATE TABLE t (id);\n".to_string(),
    );
    tree.insert(
        "db/migrations/0005_second.sql".to_string(),
        "ALTER TABLE t;\n".to_string(),
    );
    let example = example(
        "newsletter",
        "app",
        &[
            ("db/migrations/0001_sub.sql", "A\n"),
            ("db/migrations/0003_unsub.sql", "B\n"),
            ("src/sub.ts", "s\n"),
        ],
    );
    let plan = plan_apply(&tree, "app", &[example], &["newsletter"]).unwrap();
    let writes: HashMap<String, String> = plan.writes.into_iter().collect();
    assert_eq!(
        writes.get("db/migrations/0006_sub.sql").map(String::as_str),
        Some("A\n")
    );
    assert_eq!(
        writes
            .get("db/migrations/0007_unsub.sql")
            .map(String::as_str),
        Some("B\n")
    );
    assert_eq!(writes.get("src/sub.ts").map(String::as_str), Some("s\n"));
}

#[test]
fn nav_inserts_items_and_merges_icons() {
    let tree = app_tree();
    let mut example = example("photos", "app", &[("pages/photos.tsx", "p\n")]);
    example.nav = vec![NavItem {
        href: "/photos".to_string(),
        label: "Photos".to_string(),
        icon: "Camera".to_string(),
    }];
    let plan = plan_apply(&tree, "app", &[example], &["photos"]).unwrap();
    assert_eq!(plan.nav, vec!["/photos".to_string()]);
    let nav = plan
        .writes
        .iter()
        .find(|(path, _)| path == "pages/_nav.ts")
        .expect("nav write")
        .1
        .clone();
    assert!(
        nav.contains("import { Camera, House, Settings }"),
        "icons: {nav}"
    );
    assert!(
        nav.contains(
            "  { href: \"/photos\", label: \"Photos\", icon: Camera },\n  // outer:nav-items"
        ),
        "items: {nav}"
    );
}

#[test]
fn nav_skips_hrefs_already_present() {
    let mut tree = app_tree();
    tree.insert(
        "pages/_nav.ts".to_string(),
        NAV_TS.replace(
            "  // outer:nav-items",
            "  { href: \"/photos\", label: \"Pics\", icon: Camera },\n  // outer:nav-items",
        ),
    );
    let mut example = example("photos", "app", &[("pages/photos.tsx", "p\n")]);
    example.nav = vec![NavItem {
        href: "/photos".to_string(),
        label: "Photos".to_string(),
        icon: "Camera".to_string(),
    }];
    let plan = plan_apply(&tree, "app", &[example], &["photos"]).unwrap();
    assert!(plan.nav.is_empty());
    assert!(!plan.writes.iter().any(|(path, _)| path == "pages/_nav.ts"));
}

#[test]
fn nav_missing_marker_refuses_with_file_and_manual_step() {
    let mut tree = app_tree();
    tree.insert(
        "pages/_nav.ts".to_string(),
        "export const NAV = [];\n".to_string(),
    );
    let mut example = example("photos", "app", &[("pages/photos.tsx", "p\n")]);
    example.nav = vec![NavItem {
        href: "/photos".to_string(),
        label: "Photos".to_string(),
        icon: "Camera".to_string(),
    }];
    let err = plan_apply(&tree, "app", &[example], &["photos"]).unwrap_err();
    assert!(err.0.contains("pages/_nav.ts"), "unexpected: {}", err.0);
    assert!(
        err.0.contains("Without add_example"),
        "unexpected: {}",
        err.0
    );
}

#[test]
fn bindings_insert_skip_and_refuse() {
    let tree = app_tree();
    let mut example = example("photos", "app", &[("pages/photos.tsx", "p\n")]);
    example.bindings = BTreeMap::from([(
        "r2_buckets".to_string(),
        vec![BTreeMap::from([
            ("binding".to_string(), "PHOTOS".to_string()),
            ("bucket_name".to_string(), "photos".to_string()),
        ])],
    )]);
    let plan = plan_apply(&tree, "app", &[example.clone()], &["photos"]).unwrap();
    let config = plan
        .writes
        .iter()
        .find(|(path, _)| path == "void.config.ts")
        .expect("bindings write")
        .1
        .clone();
    assert!(
        config.contains("    r2_buckets: [{ binding: \"PHOTOS\", bucket_name: \"photos\" }],\n    // outer:bindings"),
        "bindings: {config}"
    );
    // Applying onto the updated tree skips the equal key (files still fresh here, so force via tree).
    let mut tree2 = tree.clone();
    tree2.insert("void.config.ts".to_string(), config.clone());
    let plan2 = plan_apply(&tree2, "app", &[example.clone()], &["photos"]).unwrap();
    assert!(plan2
        .writes
        .iter()
        .any(|(path, _)| path == "pages/photos.tsx"));
    // A different value for the same key refuses.
    let mut tree3 = tree.clone();
    tree3.insert(
        "void.config.ts".to_string(),
        VOID_CONFIG.replace(
            "    // outer:bindings",
            "    r2_buckets: [{ binding: \"OTHER\", bucket_name: \"other\" }],\n    // outer:bindings",
        ),
    );
    let err = plan_apply(&tree3, "app", &[example], &["photos"]).unwrap_err();
    assert!(err.0.contains("r2_buckets"), "unexpected: {}", err.0);
}

#[test]
fn bindings_missing_marker_refuses() {
    let mut tree = app_tree();
    tree.insert(
        "void.config.ts".to_string(),
        "export default {};\n".to_string(),
    );
    let mut ex = example("photos", "app", &[("pages/photos.tsx", "p\n")]);
    ex.bindings = BTreeMap::from([(
        "r2_buckets".to_string(),
        vec![BTreeMap::from([(
            "binding".to_string(),
            "PHOTOS".to_string(),
        )])],
    )]);
    let err = plan_apply(&tree, "app", &[ex], &["photos"]).unwrap_err();
    assert!(err.0.contains("void.config.ts"), "unexpected: {}", err.0);
}

#[test]
fn icon_names_validate() {
    assert!(is_valid_icon("Camera"));
    assert!(is_valid_icon("A1"));
    assert!(!is_valid_icon("camera"));
    assert!(!is_valid_icon("C"));
    assert!(!is_valid_icon("Has-Dash"));
    assert!(!is_valid_icon("Has Space"));
}

/// Every file under `dir` as app-relative path → text, byte-wise sorted.
fn read_tree(dir: &std::path::Path) -> BTreeMap<String, String> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel, std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    if dir.is_dir() {
        walk(dir, dir, &mut out);
    }
    out
}

/// The golden cases shared with the TypeScript engine in outer-templates
/// (`scripts/apply-fixtures/`, vendored verbatim into
/// `tests/fixtures/apply/`): each case is `tree/` + `examples/<name>/` +
/// `request.json` → `expected/` (exactly the written files) or
/// `expected-error.txt` (the refusal, byte for byte). Examples load through
/// the real `example.json` loader, so both engines read the same inputs.
#[test]
fn shared_golden_cases_match_the_typescript_engine() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/apply");
    let mut cases: Vec<_> = std::fs::read_dir(&root)
        .unwrap_or_else(|_| panic!("missing golden fixtures at {}", root.display()))
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    cases.sort();
    assert!(cases.len() >= 5, "golden cases in {}", root.display());
    for case in cases {
        let request: serde_json::Value =
            serde_json::from_slice(&std::fs::read(case.join("request.json")).unwrap()).unwrap();
        let template = request["template"].as_str().unwrap();
        let names: Vec<&str> = request["names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap())
            .collect();
        let examples: Vec<ExampleInput> = std::fs::read_dir(case.join("examples"))
            .unwrap()
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                crate::examples::load_example(&entry.path(), &name)
                    .unwrap_or_else(|| panic!("{}: example {name} fails to load", case.display()))
                    .apply_input()
            })
            .collect();
        let tree = read_tree(&case.join("tree"));
        let result = plan_apply(&tree, template, &examples, &names);
        let error_file = case.join("expected-error.txt");
        if error_file.is_file() {
            let want = std::fs::read_to_string(&error_file).unwrap();
            let got = result.expect_err(&format!("{}: expected a refusal", case.display()));
            assert_eq!(got.0.trim_end(), want.trim_end(), "{}", case.display());
        } else {
            let plan = result.unwrap_or_else(|e| panic!("{}: {e}", case.display()));
            let got: BTreeMap<String, String> = plan.writes.into_iter().collect();
            assert_eq!(got, read_tree(&case.join("expected")), "{}", case.display());
        }
    }
}
