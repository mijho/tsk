#[test]
fn skill_documents_retry_and_project_scope_refusals() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let skill = std::fs::read_to_string(root.join("skills/tsk-cli/SKILL.md"))
        .expect("CLI skill should exist");
    let agents = std::fs::read_to_string(root.join("AGENTS.md")).expect("AGENTS.md should exist");

    for term in [
        "tsk add -t",
        "tsk status",
        "tsk edit",
        "--desk",
        "-p <project>",
        "tsk help <command>",
        "cat plan.json | tsk add",
        "| 0 |",
        "| 1 |",
        "| 2 |",
        "| 3 |",
        "retry only",
        "list --all --json",
        "Human status is the user's",
        "## Refine a task",
        "propose, then write",
    ] {
        assert!(skill.contains(term), "skill should contain {term:?}");
    }
    assert!(
        skill.contains("indeterminate") || skill.contains("list"),
        "skill should describe indeterminate exit-3 recovery"
    );
    assert!(
        skill.contains("unknown or ambiguous names refuse")
            && skill.contains("existing absolute directory"),
        "skill should explain strict project destination resolution"
    );
    assert!(agents.contains("tsk add"));
    assert!(agents.contains("tsk list"));
}

#[test]
fn integration_suites_are_registered_once_with_bounded_binary_count() {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml"))
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(
        manifest["package"]
            .get("autotests")
            .and_then(|v| v.as_bool()),
        Some(false),
        "disable auto-discovery so suite modules are not linked twice"
    );
    let targets = manifest["test"].as_array_of_tables().unwrap();
    assert!(
        targets.len() <= 9,
        "keep the integration binary count bounded"
    );
    let mut registered = BTreeSet::new();
    for target in targets {
        let path = target["path"].as_str().unwrap().to_owned();
        assert!(registered.insert(path), "duplicate test target");
    }
    for suite in [
        "cli_add",
        "cli_list",
        "cli_setup_agent",
        "queue_board_loop",
        "starter_guides",
        "update_notice",
    ] {
        assert!(
            registered.contains(&format!("tests/{suite}.rs")),
            "{suite} mutates process state and needs its own executable"
        );
    }
    assert!(
        registered.contains("tests/demo_parity.rs"),
        "parity references must not compile the aggregate integration harness"
    );
    let package: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("site/package.json")).unwrap()).unwrap();
    assert!(
        package["scripts"]["parity:reference"]
            .as_str()
            .unwrap()
            .contains("--test demo_parity -- --ignored"),
        "the parity workflow must use the dedicated generator target"
    );
    let harness = fs::read_to_string(root.join("tests/integration.rs")).unwrap();
    for line in harness.lines() {
        if let Some(module) = line.strip_prefix("mod ").and_then(|s| s.strip_suffix(';')) {
            assert!(
                registered.insert(format!("tests/{module}.rs")),
                "suite {module} is compiled more than once"
            );
        }
    }
    let sources: BTreeSet<_> = fs::read_dir(root.join("tests"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| format!("tests/{}", path.file_name().unwrap().to_str().unwrap()))
        .collect();
    assert_eq!(
        registered, sources,
        "register every suite in integration.rs or Cargo.toml"
    );
}

/// The default `tsk` build pulls no server deps: they are optional, only the `sync-server`
/// feature enables them, and the `tsk-sync` binary requires it.
#[test]
fn the_sync_server_deps_stay_behind_their_feature() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest: toml_edit::DocumentMut = std::fs::read_to_string(root.join("Cargo.toml"))
        .expect("Cargo.toml")
        .parse()
        .expect("toml");
    let server_deps = ["rusqlite", "tiny_http"];
    for dep in server_deps {
        let spec = &manifest["dependencies"][dep];
        assert_eq!(
            spec["optional"].as_bool(),
            Some(true),
            "{dep} must be optional"
        );
    }
    let features = manifest["features"].as_table().expect("[features]");
    for (name, enables) in features.iter() {
        let enables: Vec<&str> = enables
            .as_array()
            .expect("feature list")
            .iter()
            .filter_map(|item| item.as_str())
            .collect();
        for dep in server_deps {
            let enabled = enables.contains(&format!("dep:{dep}").as_str());
            assert_eq!(enabled, name == "sync-server", "{dep} in feature {name}");
        }
    }
    assert!(
        manifest["features"].get("default").is_none(),
        "no default features"
    );
    let bins = manifest["bin"].as_array_of_tables().expect("[[bin]]");
    let sync = bins
        .iter()
        .find(|bin| bin["name"].as_str() == Some("tsk-sync"))
        .expect("tsk-sync bin");
    let required: Vec<&str> = sync["required-features"]
        .as_array()
        .expect("required-features")
        .iter()
        .filter_map(|item| item.as_str())
        .collect();
    assert_eq!(required, ["sync-server"]);
}
