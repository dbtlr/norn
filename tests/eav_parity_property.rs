//! Wave-2 acceptance (NRN-80): indexed-vs-scan parity property test.
//!
//! `document_fields` + the query router (NRN-78/79) claim that routing a
//! query through the derived index rather than scanning `frontmatter_json`
//! never changes results. `tests/find_index_routing.rs` and
//! `src/cache/query_documents.rs`'s `router_tests` already pin that claim
//! for hand-picked cases; this file stress-tests it with randomized
//! (deterministic-seed) vaults and queries spanning every predicate class
//! and a wide variety of frontmatter value shapes, comparing the exact same
//! query run twice — once against a vault whose fields are all declared
//! `indexed: true` (routes through `document_fields`), once against an
//! identical vault with no config at all (empty index set, always scans).
//!
//! No `proptest`/`quickcheck` dependency exists in this crate (grepped —
//! none), and none is added here per the task's no-new-deps constraint: this
//! is a small seeded PRNG (SplitMix64) driving a plain iteration loop, the
//! same shape as `src/cache.rs`'s existing incremental-vs-rebuild property
//! test.
//!
//! On a mismatch this is a **real router bug** — the test intentionally
//! does not touch the generator to dodge one; it panics with the full
//! repro (seed, generated docs, query args, both result sets).

use std::collections::BTreeMap;
use std::process::Command;

use serde_json::{Map, Value};
use tempfile::TempDir;

// ---- deterministic PRNG: SplitMix64 (no crate dependency needed) --------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn range(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// True with probability 1/denom.
    fn chance(&mut self, denom: u64) -> bool {
        self.next_u64().is_multiple_of(denom)
    }
}

// ---- frontmatter shape pool ---------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Int,
    Float,
    Bool,
    Null,
    Date,
    ArrStr,
    ArrMixed,
    ArrEmpty,
    ArrAllNull,
    Obj,
}

struct FieldSpec {
    name: &'static str,
    kind: Kind,
}

const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "str_plain",
        kind: Kind::Str,
    },
    FieldSpec {
        name: "str_wiki",
        kind: Kind::Str,
    },
    FieldSpec {
        name: "str_unicode",
        kind: Kind::Str,
    },
    FieldSpec {
        name: "int_field",
        kind: Kind::Int,
    },
    FieldSpec {
        name: "float_field",
        kind: Kind::Float,
    },
    FieldSpec {
        name: "bool_field",
        kind: Kind::Bool,
    },
    FieldSpec {
        name: "null_field",
        kind: Kind::Null,
    },
    FieldSpec {
        name: "date_field",
        kind: Kind::Date,
    },
    FieldSpec {
        name: "tags_field",
        kind: Kind::ArrStr,
    },
    FieldSpec {
        name: "arr_mixed",
        kind: Kind::ArrMixed,
    },
    FieldSpec {
        name: "arr_empty",
        kind: Kind::ArrEmpty,
    },
    FieldSpec {
        name: "arr_all_null",
        kind: Kind::ArrAllNull,
    },
    FieldSpec {
        name: "obj_plain",
        kind: Kind::Obj,
    },
];

/// A field name that never appears in any generated document or config —
/// the "totally absent" target for `--has`/`--missing`.
const PHANTOM_FIELD: &str = "zzz_never_present";

const STR_POOL: &[&str] = &[
    "alpha",
    "Bravo",
    "[[Alice]]",
    "note [[x]] here",
    "café",
    "naive",
    "charlie",
    "UPPER",
    "[[a]] and [[b]]",
    // Empty string is a valid stored shape (`scalar_candidates` filters it
    // out of query-operand generation — `field:` is a CLI parse error — but
    // it must still be exercised as frontmatter data: it's a real
    // `document_fields` row shape, distinct from an absent field).
    "",
];
const DATE_POOL: &[&str] = &["2026-01-01", "2026-06-15", "2026-12-31", "2025-03-10"];
const ARRSTR_POOL: &[&[&str]] = &[
    &["release:v1", "area:x"],
    &["release:v2"],
    &["type:note", "type:log", "extra"],
    &["alpha", "BRAVO"],
];

fn sample_value(kind: Kind, rng: &mut Rng) -> Value {
    match kind {
        Kind::Str => Value::String(STR_POOL[rng.range(STR_POOL.len())].to_string()),
        Kind::Int => Value::from([0i64, 1, 5, 42, -3, 100][rng.range(6)]),
        Kind::Float => serde_json::Number::from_f64([2.5, 3.0, -1.25, 0.0, 10.75][rng.range(5)])
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Kind::Bool => Value::Bool(rng.range(2) == 0),
        Kind::Null => Value::Null,
        Kind::Date => Value::String(DATE_POOL[rng.range(DATE_POOL.len())].to_string()),
        Kind::ArrStr => Value::Array(
            ARRSTR_POOL[rng.range(ARRSTR_POOL.len())]
                .iter()
                .map(|s| Value::String(s.to_string()))
                .collect(),
        ),
        Kind::ArrMixed => match rng.range(3) {
            0 => vec![Value::String("a".into()), Value::from(1), Value::Bool(true)],
            1 => vec![Value::String("x".into()), Value::String("y".into())],
            _ => vec![Value::from(1), Value::from(2), Value::from(3)],
        }
        .into(),
        Kind::ArrEmpty => Value::Array(vec![]),
        Kind::ArrAllNull => Value::Array(vec![Value::Null, Value::Null]),
        Kind::Obj => match rng.range(2) {
            0 => serde_json::json!({"name": "Alice"}),
            _ => serde_json::json!({"name": "[[Bob]]"}),
        },
    }
}

/// Flatten a value into scalar CLI-operand text candidates: the value itself
/// if it's a string/number/bool, or (one level of) its array elements.
/// Empty strings are excluded — `field:` is a CLI parse error, not a
/// meaningful predicate.
fn scalar_candidates(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if !s.is_empty() => out.push(s.clone()),
        Value::Number(n) => out.push(n.to_string()),
        Value::Bool(b) => out.push(b.to_string()),
        Value::Array(items) => {
            for item in items {
                scalar_candidates(item, out);
            }
        }
        _ => {}
    }
}

fn mutate(s: &str, rng: &mut Rng) -> String {
    match rng.range(6) {
        0 => s.to_uppercase(),
        1 => s.to_lowercase(),
        2 => format!("[[{s}]]"),
        3 => s.replace("[[", "").replace("]]", ""),
        4 => {
            let half = (s.chars().count() / 2).max(1);
            s.chars().take(half).collect()
        }
        _ => format!("{s}_mut"),
    }
}

// ---- vault generation ----------------------------------------------------

/// `None` frontmatter means "malformed" — the file gets a YAML block that
/// fails to parse (an unclosed flow sequence), producing an all-sentinel
/// document per `document_fields::insert_rows`'s `frontmatter: None` arm.
struct GeneratedDoc {
    name: String,
    frontmatter: Option<BTreeMap<String, Value>>,
}

fn generate_vault(rng: &mut Rng) -> Vec<GeneratedDoc> {
    let doc_count = 4 + rng.range(5); // 4..=8
    let malformed_idx = if doc_count >= 3 && rng.chance(6) {
        Some(rng.range(doc_count))
    } else {
        None
    };
    (0..doc_count)
        .map(|i| {
            let name = format!("doc{i:02}.md");
            if Some(i) == malformed_idx {
                return GeneratedDoc {
                    name,
                    frontmatter: None,
                };
            }
            let mut fm = BTreeMap::new();
            for field in FIELDS {
                if rng.chance(2) {
                    fm.insert(field.name.to_string(), sample_value(field.kind, rng));
                }
            }
            GeneratedDoc {
                name,
                frontmatter: Some(fm),
            }
        })
        .collect()
}

fn frontmatter_yaml(fm: &BTreeMap<String, Value>) -> String {
    let map: Map<String, Value> = fm.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    serde_yaml::to_string(&Value::Object(map)).expect("generated frontmatter should serialize")
}

fn write_vault(root: &std::path::Path, docs: &[GeneratedDoc], indexed: bool) {
    std::fs::create_dir_all(root).unwrap();
    for doc in docs {
        let body = match &doc.frontmatter {
            Some(fm) => format!("---\n{}---\nbody\n", frontmatter_yaml(fm)),
            // Unclosed flow sequence: `serde_yaml` fails to parse this, so
            // `extract_frontmatter` reports `frontmatter-parse-failed` and
            // treats the document as having no frontmatter at all.
            None => "---\nbroken: [1, 2\n---\nbody\n".to_string(),
        };
        std::fs::write(root.join(&doc.name), body).unwrap();
    }
    if indexed {
        let mut fields: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for doc in docs {
            if let Some(fm) = &doc.frontmatter {
                fields.extend(fm.keys().map(String::as_str));
            }
        }
        let mut yaml = String::from("validate:\n  rules:\n    - name: r\n      field_types:\n");
        for field in fields {
            yaml.push_str(&format!("        {field}: {{ indexed: true }}\n"));
        }
        let dir = root.join(".norn");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), yaml).unwrap();
    }
}

// ---- query generation -----------------------------------------------------

/// All scalar text candidates stored anywhere in the vault for `field`,
/// deduplicated, in generation order.
fn candidates_for(docs: &[GeneratedDoc], field: &str) -> Vec<String> {
    let mut out = Vec::new();
    for doc in docs {
        if let Some(fm) = &doc.frontmatter {
            if let Some(v) = fm.get(field) {
                scalar_candidates(v, &mut out);
            }
        }
    }
    out.dedup();
    out
}

fn string_field_names() -> Vec<&'static str> {
    FIELDS
        .iter()
        .filter(|f| matches!(f.kind, Kind::Str | Kind::ArrStr))
        .map(|f| f.name)
        .collect()
}

/// Build one randomized `norn find` predicate against `docs`. Returns
/// `None` on the rare iteration where the chosen predicate class has no
/// usable field (falls back to `--has`/`--missing`, which always works).
fn build_query_args(docs: &[GeneratedDoc], rng: &mut Rng) -> Vec<String> {
    let class = rng.range(12);
    match class {
        // --eq / --not-eq
        0 | 1 => {
            let flag = if class == 0 { "--eq" } else { "--not-eq" };
            for _ in 0..FIELDS.len() {
                let field = FIELDS[rng.range(FIELDS.len())].name;
                let cands = candidates_for(docs, field);
                if cands.is_empty() {
                    continue;
                }
                let base = &cands[rng.range(cands.len())];
                let value = if rng.chance(2) {
                    base.clone()
                } else {
                    mutate(base, rng)
                };
                if value.is_empty() {
                    continue;
                }
                return vec![flag.to_string(), format!("{field}:{value}")];
            }
            vec!["--has".to_string(), PHANTOM_FIELD.to_string()]
        }
        // --in / --not-in
        2 | 3 => {
            let flag = if class == 2 { "--in" } else { "--not-in" };
            for _ in 0..FIELDS.len() {
                let field = FIELDS[rng.range(FIELDS.len())].name;
                let cands = candidates_for(docs, field);
                if cands.is_empty() {
                    continue;
                }
                let n = 1 + rng.range(3.min(cands.len().max(1)));
                let values: Vec<String> = (0..n)
                    .map(|_| {
                        let base = &cands[rng.range(cands.len())];
                        if rng.chance(2) {
                            base.clone()
                        } else {
                            mutate(base, rng)
                        }
                    })
                    .filter(|v| !v.is_empty())
                    .collect();
                if values.is_empty() {
                    continue;
                }
                return vec![flag.to_string(), format!("{field}:{}", values.join(","))];
            }
            vec!["--has".to_string(), PHANTOM_FIELD.to_string()]
        }
        // --has
        4 => {
            let field = if rng.chance(3) {
                PHANTOM_FIELD
            } else {
                FIELDS[rng.range(FIELDS.len())].name
            };
            vec!["--has".to_string(), field.to_string()]
        }
        // --missing
        5 => {
            let field = if rng.chance(3) {
                PHANTOM_FIELD
            } else {
                FIELDS[rng.range(FIELDS.len())].name
            };
            vec!["--missing".to_string(), field.to_string()]
        }
        // --before / --after / --on (date_field only)
        6..=8 => {
            let flag = match class {
                6 => "--before",
                7 => "--after",
                _ => "--on",
            };
            let value = if rng.chance(2) {
                DATE_POOL[rng.range(DATE_POOL.len())].to_string()
            } else {
                "today".to_string()
            };
            vec![flag.to_string(), format!("date_field:{value}")]
        }
        // --starts-with / --ends-with / --contains
        _ => {
            let flag = match class {
                9 => "--starts-with",
                10 => "--ends-with",
                _ => "--contains",
            };
            let fields = string_field_names();
            for _ in 0..fields.len() {
                let field = fields[rng.range(fields.len())];
                let cands = candidates_for(docs, field);
                let cands: Vec<&String> = cands.iter().filter(|c| !c.is_empty()).collect();
                if cands.is_empty() {
                    continue;
                }
                let base = cands[rng.range(cands.len())];
                let chars: Vec<char> = base.chars().collect();
                let len = (chars.len() / 2).max(1).min(chars.len());
                let needle: String = match class {
                    9 => chars[..len].iter().collect(),
                    10 => chars[chars.len() - len..].iter().collect(),
                    _ => {
                        let start = if chars.len() > 1 {
                            rng.range(chars.len() - 1)
                        } else {
                            0
                        };
                        let end = (start + len).min(chars.len()).max(start + 1);
                        chars[start..end].iter().collect()
                    }
                };
                let needle = if rng.chance(3) {
                    mutate(&needle, rng)
                } else {
                    needle
                };
                if needle.is_empty() {
                    continue;
                }
                return vec![flag.to_string(), format!("{field}:{needle}")];
            }
            vec!["--has".to_string(), PHANTOM_FIELD.to_string()]
        }
    }
}

// ---- CLI execution --------------------------------------------------------

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

fn run_find(root: &std::path::Path, args: &[String]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.arg("-C").arg(root).arg("find");
    command.args(args);
    command.args(["--no-limit", "--format", "json"]);
    let cache_dir = tempfile::tempdir().unwrap();
    command.env("XDG_CACHE_HOME", cache_dir.path());
    command.env("XDG_STATE_HOME", cache_dir.path().join("state"));
    prewrite_prune_marker(cache_dir.path());
    let output = command.output().expect("norn find should run");
    assert!(
        output.status.success(),
        "norn find failed\nargs: {args:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).expect("find --format json output should be valid JSON")
}

fn describe_docs(docs: &[GeneratedDoc]) -> String {
    docs.iter()
        .map(|d| match &d.frontmatter {
            Some(fm) => format!("  {}: {:?}", d.name, fm),
            None => format!("  {}: <malformed frontmatter>", d.name),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn indexed_and_scan_routes_agree_across_predicate_classes_and_value_shapes() {
    const ITERATIONS: u64 = 220;
    const BASE_SEED: u64 = 0xA5F0_1234_9E37_79B9;

    for i in 0..ITERATIONS {
        let seed = BASE_SEED.wrapping_add(i);
        let mut rng = Rng::new(seed);
        let docs = generate_vault(&mut rng);
        let args = build_query_args(&docs, &mut rng);

        let indexed_root = TempDir::new().unwrap();
        let scan_root = TempDir::new().unwrap();
        write_vault(indexed_root.path(), &docs, /* indexed */ true);
        write_vault(scan_root.path(), &docs, /* indexed */ false);

        let indexed_result = run_find(indexed_root.path(), &args);
        let scan_result = run_find(scan_root.path(), &args);

        if indexed_result != scan_result {
            panic!(
                "indexed-vs-scan parity mismatch at iteration {i} (seed {seed:#x})\n\
                 query args: {args:?}\n\
                 generated docs:\n{}\n\
                 indexed (routed) result:\n{}\n\
                 scan result:\n{}\n",
                describe_docs(&docs),
                serde_json::to_string_pretty(&indexed_result).unwrap(),
                serde_json::to_string_pretty(&scan_result).unwrap(),
            );
        }
    }

    eprintln!("eav_parity_property: {ITERATIONS} iterations, 0 failures");
}
