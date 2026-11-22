use super::session::resolve_qoder_title;
use super::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

#[test]
fn active_branch_on_fixture_keeps_leaf_and_drops_abandoned_sibling() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/qoder/projects/-tmp-demo/11111111-1111-4111-8111-111111111111.jsonl");
    let file = fs::File::open(&fixture).unwrap();
    let parsed = parse_claude_records(BufReader::new(file), &fixture, ClaudeFlavor::Qoder)
        .unwrap()
        .unwrap();
    let branch = active_branch(&parsed.records_by_uuid, &parsed.active_leaves);
    assert_eq!(
        branch,
        vec!["u-root-1", "att-1", "a-kept-1", "u-kept-tool-1", "a-kept-2"]
    );
}

#[test]
fn active_branch_without_leaf_falls_back_to_latest_timestamp() {
    let records = vec![
        ("root".to_string(), None),
        ("branch-a".to_string(), Some("root".to_string())),
        ("branch-b".to_string(), Some("root".to_string())),
    ];
    let mut timestamps = HashMap::new();
    timestamps.insert("root".to_string(), 1000);
    timestamps.insert("branch-a".to_string(), 3000);
    timestamps.insert("branch-b".to_string(), 2000);

    let branch = active_branch_with_timestamps(&records, &timestamps, &[]);
    assert_eq!(branch, vec!["root", "branch-a"]);
}

#[test]
fn active_branch_terminates_on_parent_cycle() {
    let records = vec![
        ("n1".to_string(), Some("n2".to_string())),
        ("n2".to_string(), Some("n1".to_string())),
    ];
    let branch = active_branch(&records, &["n2".to_string()]);
    assert_eq!(branch, vec!["n1", "n2"]);
}

#[test]
fn scan_fixture_emits_main_session_on_active_branch() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder");
    let dir = TempDir::new().unwrap();
    let ctx = ScanContext::with_roots(dir.path().to_path_buf(), vec![ScanRoot::local(root)], None);
    let convs = QoderConnector::new().scan(&ctx).unwrap();

    let main = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some("11111111-1111-4111-8111-111111111111"))
        .expect("main fixture conversation");
    assert_eq!(main.agent_slug, "qoder");
    assert_eq!(main.metadata["source"], "qoder_jsonl");
    assert_eq!(main.metadata["is_subagent"], false);
    assert_eq!(main.metadata["model"], "qmodel-demo");
    assert_eq!(main.workspace, Some(PathBuf::from("/tmp/demo")));

    let contents: Vec<&str> = main.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents.len(), 4);
    assert!(contents[0].contains("Inspect the demo module"));
    assert!(contents[1].contains("Checking demo module."));
    assert_eq!(main.messages[2].role, "tool");
    assert!(contents[2].contains("Subagent finished checking demo"));
    assert!(contents[3].contains("All checks passed on kept branch."));
    assert!(
        !contents.iter().any(|c| c.contains("Abandoned")),
        "abandoned branch must not be emitted: {contents:?}"
    );
}

#[test]
fn projects_root_resolved_prefers_qoder_home_then_home_then_relative() {
    let explicit = Path::new("/opt/qoder_home");
    let home = Path::new("/home/jane");
    assert_eq!(
        QoderConnector::projects_root_resolved(Some(explicit), Some(home)),
        PathBuf::from("/opt/qoder_home/projects")
    );
    assert_eq!(
        QoderConnector::projects_root_resolved(None, Some(home)),
        PathBuf::from("/home/jane/.qoder/projects")
    );
    assert_eq!(
        QoderConnector::projects_root_resolved(None, None),
        PathBuf::from(".qoder/projects")
    );
}

#[test]
fn projects_roots_resolved_covers_all_four_families_and_qoder_projects_dir() {
    let explicit_projects = Path::new("/custom/qoder/projects");
    let explicit_home = Path::new("/opt/qoder_home");
    let home = Path::new("/home/jane");
    let appdata = Path::new("/win/Users/jane/AppData/Roaming");
    let xdg = Path::new("/home/jane/.xdg-config");

    assert_eq!(
        QoderConnector::projects_roots_resolved(
            Some(explicit_projects),
            Some(explicit_home),
            Some(home),
            None,
            Some(appdata),
            Some(xdg),
        ),
        vec![PathBuf::from("/custom/qoder/projects")]
    );

    assert_eq!(
        QoderConnector::projects_roots_resolved(
            None,
            Some(explicit_home),
            Some(home),
            None,
            Some(appdata),
            Some(xdg),
        ),
        vec![PathBuf::from("/opt/qoder_home/projects")]
    );

    let roots = QoderConnector::projects_roots_resolved(
        None,
        None,
        Some(home),
        None,
        Some(appdata),
        Some(xdg),
    );
    assert!(roots.contains(&home.join(".qoder/projects")));
    assert!(roots.contains(&home.join(".qoderwork/projects")));
    assert!(roots.contains(&home.join(".qoder-cn/projects")));
    assert!(
        roots.contains(
            &home.join("Library/Application Support/Qoder/SharedClientCache/cli/projects")
        )
    );
    assert!(roots.contains(&home.join(".config/Qoder/SharedClientCache/cli/projects")));
    assert!(roots.contains(&home.join("AppData/Roaming/Qoder/SharedClientCache/cli/projects")));
    assert!(roots.contains(&appdata.join("Qoder/SharedClientCache/cli/projects")));
    assert!(roots.contains(&xdg.join("Qoder/SharedClientCache/cli/projects")));
}

#[test]
fn scan_without_local_default_stores_scans_nothing() {
    let dir = TempDir::new().unwrap();
    let ctx =
        ScanContext::local_default(dir.path().to_path_buf(), None).without_local_default_stores();
    let convs = QoderConnector::new().scan(&ctx).unwrap();
    assert!(convs.is_empty());
    let discovered = QoderConnector::new().discover_source_files(&ctx).unwrap();
    assert!(discovered.is_empty());
}

#[test]
fn scan_fixture_emits_subagent_with_sidecar_metadata() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder");
    let dir = TempDir::new().unwrap();
    let ctx = ScanContext::with_roots(dir.path().to_path_buf(), vec![ScanRoot::local(root)], None);
    let convs = QoderConnector::new().scan(&ctx).unwrap();

    let subagent = convs
        .iter()
        .find(|c| {
            c.external_id.as_deref()
                == Some("11111111-1111-4111-8111-111111111111/agent-ageneral-purpose-0001")
        })
        .expect("subagent fixture conversation");
    assert_eq!(subagent.agent_slug, "qoder");
    assert_eq!(subagent.metadata["source"], "qoder_jsonl");
    assert_eq!(subagent.metadata["is_subagent"], true);
    assert_eq!(
        subagent.metadata["parent_id"],
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(subagent.metadata["agent_type"], "general-purpose");
    assert_eq!(subagent.metadata["description"], "Explore demo");
    assert_eq!(subagent.metadata["tool_use_id"], "call-subagent-1");
    assert_eq!(subagent.metadata["agent_id"], "ageneral-purpose-0001");
    assert_eq!(subagent.metadata["model"], "qmodel-demo");

    let contents: Vec<&str> = subagent
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(contents.len(), 2);
    assert!(contents[0].contains("Inspect demo module files"));
    assert!(contents[1].contains("Found 2 files in demo."));
    assert!(
        !contents.iter().any(|c| c.contains("Stale subagent branch")),
        "stale subagent branch must not be emitted: {contents:?}"
    );
}

#[test]
fn subagent_with_corrupt_meta_still_emits_without_agent_type() {
    let dir = TempDir::new().unwrap();
    let subagents_dir = dir
        .path()
        .join("projects/-tmp-corrupt/22222222-2222-4222-8222-222222222222/subagents");
    fs::create_dir_all(&subagents_dir).unwrap();
    fs::write(
        subagents_dir.join("agent-acustom-0002.jsonl"),
        r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/tmp/corrupt","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Hello subagent"}}"#,
    )
    .unwrap();
    fs::write(
        subagents_dir.join("agent-acustom-0002.meta.json"),
        "{not valid json",
    )
    .unwrap();

    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().join("projects"))],
        None,
    );
    let convs = QoderConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let subagent = &convs[0];
    assert_eq!(
        subagent.external_id.as_deref(),
        Some("22222222-2222-4222-8222-222222222222/agent-acustom-0002")
    );
    assert_eq!(subagent.metadata["is_subagent"], true);
    assert_eq!(
        subagent.metadata["parent_id"],
        "22222222-2222-4222-8222-222222222222"
    );
    assert!(subagent.metadata.get("agent_type").is_none());
}

#[test]
fn subagent_with_missing_parent_session_still_emits() {
    let dir = TempDir::new().unwrap();
    let subagents_dir = dir
        .path()
        .join("projects/-tmp-orphan/33333333-3333-4333-8333-333333333333/subagents");
    fs::create_dir_all(&subagents_dir).unwrap();
    fs::write(
        subagents_dir.join("agent-aorphan-0003.jsonl"),
        r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/tmp/orphan","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Orphan subagent"}}"#,
    )
    .unwrap();

    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().join("projects"))],
        None,
    );
    let convs = QoderConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].metadata["is_subagent"], true);
    assert_eq!(
        convs[0].metadata["parent_id"],
        "33333333-3333-4333-8333-333333333333"
    );
}

#[test]
fn main_session_files_never_returns_subagent_files_while_discover_includes_both() {
    let projects_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder/projects");
    let main_files = QoderConnector::main_session_files(&projects_root);
    assert_eq!(main_files.len(), 1);
    assert!(
        main_files
            .iter()
            .all(|p| !QoderConnector::is_subagent_transcript_path(p)),
        "main_session_files must exclude subagents: {main_files:?}"
    );

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder");
    let dir = TempDir::new().unwrap();
    let ctx = ScanContext::with_roots(dir.path().to_path_buf(), vec![ScanRoot::local(root)], None);
    let discovered = QoderConnector::new().discover_source_files(&ctx).unwrap();
    assert_eq!(discovered.len(), 2);
    assert!(
        discovered
            .iter()
            .any(|d| QoderConnector::is_subagent_transcript_path(&d.source_path)),
        "discover_source_files should include subagent transcript"
    );
}

#[test]
fn project_named_subagents_is_discovered_and_scanned_as_main_session() {
    let dir = TempDir::new().unwrap();
    let projects = dir.path().join("projects");
    let main_in_subagents_proj = projects
        .join("subagents")
        .join("11111111-1111-4111-8111-111111111111.jsonl");
    let stray_agent_in_proj = projects.join("-tmp-demo").join("agent-123.jsonl");
    let stray_nested = projects.join("-tmp-demo").join("notes").join("stray.jsonl");
    fs::create_dir_all(main_in_subagents_proj.parent().unwrap()).unwrap();
    fs::create_dir_all(stray_nested.parent().unwrap()).unwrap();
    let line = r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/work/subagents","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Main session in subagents project"}}"#;
    fs::write(&main_in_subagents_proj, line).unwrap();
    fs::write(&stray_agent_in_proj, line).unwrap();
    fs::write(&stray_nested, line).unwrap();

    assert!(!QoderConnector::is_subagent_transcript_path(
        &main_in_subagents_proj
    ));
    let main_files = QoderConnector::main_session_files(&projects);
    assert_eq!(main_files, vec![main_in_subagents_proj.clone()]);

    let ctx = ScanContext::with_roots(
        dir.path().join("state"),
        vec![ScanRoot::local(projects)],
        None,
    )
    .without_local_default_stores();
    let convs = QoderConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].external_id.as_deref(),
        Some("11111111-1111-4111-8111-111111111111")
    );
    assert_eq!(convs[0].metadata["is_subagent"], false);
    assert_eq!(convs[0].source_path, main_in_subagents_proj);
}

#[test]
fn flat_shared_client_cache_layout_with_dotted_stem_is_discovered_and_scanned() {
    let dir = TempDir::new().unwrap();
    let cli_projects = dir.path().join("Qoder/SharedClientCache/cli/projects");
    fs::create_dir_all(&cli_projects).unwrap();
    let stem = "task-10f849241e06451d9c9c.session.execution";
    let flat_path = cli_projects.join(format!("{stem}.jsonl"));
    let flat_agent = cli_projects.join("agent-stray.jsonl");
    fs::write(
        &flat_path,
        r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/work/cli","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"SharedClientCache task"}}"#,
    )
    .unwrap();
    fs::write(
        &flat_agent,
        r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Should skip"}}"#,
    )
    .unwrap();

    let ctx = ScanContext::with_roots(
        dir.path().join("state"),
        vec![ScanRoot::local(cli_projects)],
        None,
    )
    .without_local_default_stores();
    let discovered = QoderConnector::new().discover_source_files(&ctx).unwrap();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].source_path, flat_path);

    let convs = QoderConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some(stem));
    assert_eq!(convs[0].metadata["is_subagent"], false);
    assert_eq!(convs[0].workspace, Some(PathBuf::from("/work/cli")));
}

#[test]
#[allow(clippy::too_many_lines)]
fn companion_session_json_sidecar_backfills_workspace_metadata_and_boundaries() {
    let dir = TempDir::new().unwrap();
    let projects = dir.path().join("projects");
    let proj_dir = projects.join("-tmp-proj");
    fs::create_dir_all(&proj_dir).unwrap();
    let stem = "22222222-2222-4222-8222-222222222222";
    let transcript = proj_dir.join(format!("{stem}.jsonl"));
    let sidecar = proj_dir.join(format!("{stem}-session.json"));

    // Transcript has NO cwd and includes an ai-title that sidecar title must outrank.
    fs::write(
        &transcript,
        concat!(
            r#"{"type":"ai-title","aiTitle":"Generated title"}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"forked prompt"}}"#,
            "\n",
        ),
    )
    .unwrap();
    fs::write(
        &sidecar,
        r#"{"title":"Forked work","parent_session_id":"00000000-0000-4000-8000-000000000000","fork_from":"11111111-1111-4111-8111-111111111111","working_dir":"/tmp/qoder-sidecar"}"#,
    )
    .unwrap();

    // Set transcript mtime old, sidecar mtime recent -> since_ts filtering must include it!
    let old_time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let new_time = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
    fs::OpenOptions::new()
        .write(true)
        .open(&transcript)
        .unwrap()
        .set_modified(old_time)
        .unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&sidecar)
        .unwrap()
        .set_modified(new_time)
        .unwrap();

    let since_ts_ms =
        i64::try_from(new_time.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap() - 5_000;

    let ctx = ScanContext::with_roots(
        dir.path().join("state"),
        vec![ScanRoot::local(projects)],
        Some(since_ts_ms),
    )
    .without_local_default_stores();

    let discovered = QoderConnector::new().discover_source_files(&ctx).unwrap();
    assert_eq!(discovered.len(), 2);
    assert!(
        discovered
            .iter()
            .any(|d| d.source_path == transcript
                && d.role == DiscoveredSourceRole::PrimarySessionLog)
    );
    assert!(
        discovered
            .iter()
            .any(|d| d.source_path == sidecar && d.role == DiscoveredSourceRole::MetadataSidecar)
    );

    let mut completions = Vec::new();
    let mut convs = Vec::new();
    {
        let mut on_complete = |c: &SourceCompletion| {
            completions.push(c.clone());
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut on_complete),
        };
        QoderConnector::new()
            .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conv| {
                convs.push(conv);
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.title.as_deref(), Some("Forked work"));
    assert_eq!(conv.workspace, Some(PathBuf::from("/tmp/qoder-sidecar")));
    assert_eq!(
        conv.metadata["fork_from"],
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(
        conv.metadata["parent_session_id"],
        "00000000-0000-4000-8000-000000000000"
    );

    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].source.source_path, transcript);
    assert_eq!(completions[0].required_sidecars.len(), 1);
    assert_eq!(completions[0].required_sidecars[0].source_path, sidecar);

    // If neither transcript nor sidecar was modified after since_ts, nothing is scanned.
    let future_since_ms = i64::try_from(
        (SystemTime::now() + Duration::from_secs(3600))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let ctx_future = ScanContext::with_roots(
        dir.path().join("state"),
        vec![ScanRoot::local(dir.path().join("projects"))],
        Some(future_since_ms),
    )
    .without_local_default_stores();
    assert!(
        QoderConnector::new()
            .discover_source_files(&ctx_future)
            .unwrap()
            .is_empty()
    );
    assert!(QoderConnector::new().scan(&ctx_future).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn companion_session_json_rejects_symlink_and_falls_back_gracefully() {
    let dir = TempDir::new().unwrap();
    let projects = dir.path().join("projects/proj");
    fs::create_dir_all(&projects).unwrap();
    let outside = dir.path().join("secret-sidecar.json");
    fs::write(&outside, r#"{"title":"Leaked","working_dir":"/secret"}"#).unwrap();

    let stem = "33333333-3333-4333-8333-333333333333";
    let transcript = projects.join(format!("{stem}.jsonl"));
    let sidecar_link = projects.join(format!("{stem}-session.json"));
    fs::write(
        &transcript,
        r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Prompt fallback"}}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, &sidecar_link).unwrap();

    let ctx = ScanContext::with_roots(
        dir.path().join("state"),
        vec![ScanRoot::local(dir.path().join("projects"))],
        None,
    )
    .without_local_default_stores();
    let convs = QoderConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("Prompt fallback"));
    assert!(convs[0].workspace.is_none());
}

#[test]
fn companion_session_json_does_not_overwrite_transcript_cwd_or_emit_blank_links() {
    let dir = TempDir::new().unwrap();
    let projects = dir.path().join("projects/proj");
    fs::create_dir_all(&projects).unwrap();
    let stem = "44444444-4444-4444-8444-444444444444";
    let transcript = projects.join(format!("{stem}.jsonl"));
    let sidecar = projects.join(format!("{stem}-session.json"));
    fs::write(
        &transcript,
        r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/transcript/cwd","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Keep transcript cwd"}}"#,
    )
    .unwrap();
    fs::write(
        &sidecar,
        r#"{"title":"   ","parent_session_id":"  ","fork_from":"","working_dir":"/sidecar/cwd"}"#,
    )
    .unwrap();

    let ctx = ScanContext::with_roots(
        dir.path().join("state"),
        vec![ScanRoot::local(dir.path().join("projects"))],
        None,
    )
    .without_local_default_stores();
    let convs = QoderConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].workspace, Some(PathBuf::from("/transcript/cwd")));
    assert_eq!(convs[0].title.as_deref(), Some("Keep transcript cwd"));
    assert!(convs[0].metadata.get("fork_from").is_none());
    assert!(convs[0].metadata.get("parent_session_id").is_none());
}

#[test]
fn title_database_resolution_strictly_binds_home_default_roots_only() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let app_data = dir.path().join("appdata");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&app_data).unwrap();

    let intl_root = home.join(".qoder/projects");
    let cn_root = home.join(".qoder-cn/projects");
    assert_eq!(
        qoder_title_database_for_root_resolved(&intl_root, Some(&home), Some(&app_data)),
        Some(app_data.join("com.qoder.app.stable/main.sqlite"))
    );
    assert_eq!(
        qoder_title_database_for_root_resolved(&cn_root, Some(&home), Some(&app_data)),
        Some(app_data.join("com.qodercn.app.stable/main.sqlite"))
    );

    for non_bound in [
        home.join(".qoderwork/projects"),
        home.join("sessions/projects"),
        home.join("Library/Application Support/Qoder/SharedClientCache/cli/projects"),
        home.join(".qoder"),
        home.join("mirror-0001/.qoder/projects"),
        PathBuf::from(".qoder/projects"),
    ] {
        assert_eq!(
            qoder_title_database_for_root_resolved(&non_bound, Some(&home), Some(&app_data)),
            None,
            "only home-anchored .qoder and .qoder-cn may bind main.sqlite: {}",
            non_bound.display()
        );
    }

    // Transcript path resolution including symlinked default root.
    #[cfg(unix)]
    {
        let target = dir.path().join("moved-cn-projects");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(cn_root.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &cn_root).unwrap();
        let resolved_cn = fs::canonicalize(&cn_root).unwrap();
        assert_eq!(
            qoder_title_database_for_path_resolved(
                &resolved_cn.join("proj/aaaa.jsonl"),
                Some(&home),
                Some(&app_data),
            ),
            Some(app_data.join("com.qodercn.app.stable/main.sqlite"))
        );
    }

    assert_eq!(
        qoder_title_database_for_path_resolved(
            &intl_root.join("-Users-u-demo/aaaa.jsonl"),
            Some(&home),
            Some(&app_data),
        ),
        Some(app_data.join("com.qoder.app.stable/main.sqlite"))
    );
    assert_eq!(
        qoder_title_database_for_path_resolved(
            &home.join(".qoderwork/projects/aaaa.jsonl"),
            Some(&home),
            Some(&app_data),
        ),
        None
    );
}

#[test]
fn title_precedence_enforces_db_then_sidecar_then_transcript_then_prompt() {
    assert_eq!(
        resolve_qoder_title(
            Some("From database"),
            Some("From sidecar"),
            Some("Renamed in chat"),
            &[],
            None,
        )
        .as_deref(),
        Some("From database")
    );
    assert_eq!(
        resolve_qoder_title(
            Some("   "),
            Some("From sidecar"),
            Some("Renamed in chat"),
            &[],
            None,
        )
        .as_deref(),
        Some("From sidecar")
    );
    assert_eq!(
        resolve_qoder_title(
            None,
            Some("\u{0000}\u{0001}\u{0080}"),
            Some("Renamed in chat"),
            &[],
            None,
        )
        .as_deref(),
        Some("\u{0000}\u{0001}\u{0080}")
    );
    assert_eq!(
        resolve_qoder_title(None, Some("   "), Some("Renamed in chat"), &[], None).as_deref(),
        Some("Renamed in chat")
    );
}

#[cfg(any(
    feature = "cursor",
    feature = "opencode",
    feature = "devin",
    feature = "qoder-sqlite"
))]
#[test]
#[allow(clippy::too_many_lines)]
fn main_sqlite_title_store_precedence_and_hermeticity() {
    use crate::connectors::sqlite_sync::Connection;

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("com.qoder.app.stable/main.sqlite");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let sid_db_wins = "11111111-1111-4111-8111-111111111111";
    let sid_blank_db = "22222222-2222-4222-8222-222222222222";
    let sid_missing_db = "33333333-3333-4333-8333-333333333333";
    let sid_ai_title = "44444444-4444-4444-8444-444444444444";
    let sid_prompt = "55555555-5555-4555-8555-555555555555";

    {
        let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
        conn.execute_batch("CREATE TABLE chat_sessions (session_id TEXT PRIMARY KEY, title TEXT);")
            .unwrap();
        conn.execute(&format!(
            "INSERT INTO chat_sessions (session_id, title) VALUES \
             ('{sid_db_wins}', 'From database'), \
             ('{sid_blank_db}', '   '), \
             ('null-title', NULL);"
        ))
        .unwrap();
    }

    let loaded = load_qoder_title_map_from_db(&db_path);
    assert_eq!(
        loaded.get(sid_db_wins).map(String::as_str),
        Some("From database")
    );
    assert!(!loaded.contains_key(sid_blank_db));
    assert!(!loaded.contains_key("null-title"));

    // Corrupt DB and DB missing chat_sessions table return empty map gracefully.
    let corrupt_db = dir.path().join("corrupt.sqlite");
    fs::write(&corrupt_db, "not sqlite").unwrap();
    assert!(load_qoder_title_map_from_db(&corrupt_db).is_empty());

    let empty_schema_db = dir.path().join("other-table.sqlite");
    {
        let conn = Connection::open(empty_schema_db.to_string_lossy().as_ref()).unwrap();
        conn.execute_batch("CREATE TABLE unrelated (id TEXT);")
            .unwrap();
    }
    assert!(load_qoder_title_map_from_db(&empty_schema_db).is_empty());

    #[cfg(unix)]
    {
        let linked_db = dir.path().join("linked-main.sqlite");
        std::os::unix::fs::symlink(&db_path, &linked_db).unwrap();
        assert!(load_qoder_title_map_from_db(&linked_db).is_empty());
    }

    // End-to-end scan through title resolver enforcing exact precedence:
    // main.sqlite > -session.json > custom-title > ai-title > first user prompt.
    let projects = dir.path().join("projects/proj");
    fs::create_dir_all(&projects).unwrap();

    fs::write(
        projects.join(format!("{sid_db_wins}.jsonl")),
        concat!(
            r#"{"type":"custom-title","customTitle":"Renamed in chat"}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 1"}}"#,
            "\n",
        ),
    )
    .unwrap();
    fs::write(
        projects.join(format!("{sid_db_wins}-session.json")),
        r#"{"title":"From sidecar"}"#,
    )
    .unwrap();

    fs::write(
        projects.join(format!("{sid_blank_db}.jsonl")),
        concat!(
            r#"{"type":"custom-title","customTitle":"Renamed in chat"}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 2"}}"#,
            "\n",
        ),
    )
    .unwrap();
    fs::write(
        projects.join(format!("{sid_blank_db}-session.json")),
        r#"{"title":"From sidecar"}"#,
    )
    .unwrap();

    fs::write(
        projects.join(format!("{sid_missing_db}.jsonl")),
        concat!(
            r#"{"type":"ai-title","aiTitle":"Generated AI title"}"#,
            "\n",
            r#"{"type":"custom-title","customTitle":"Renamed in chat"}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 3"}}"#,
            "\n",
        ),
    )
    .unwrap();

    fs::write(
        projects.join(format!("{sid_ai_title}.jsonl")),
        concat!(
            r#"{"type":"ai-title","aiTitle":"Generated AI title"}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 4"}}"#,
            "\n",
        ),
    )
    .unwrap();

    fs::write(
        projects.join(format!("{sid_prompt}.jsonl")),
        r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 5"}}"#,
    )
    .unwrap();

    // With allow_local_default_stores = true and explicit resolver pointing at our temp main.sqlite:
    let ctx = ScanContext::with_roots(
        dir.path().join("state"),
        vec![ScanRoot::local(dir.path().join("projects"))],
        None,
    );
    let mut convs = Vec::new();
    let db_for_closure = db_path.clone();
    scan_qoder_with_title_resolver(
        &ctx,
        &[],
        &mut SourceScanHooks::default(),
        &move |_| Some(db_for_closure.clone()),
        &mut |c| {
            convs.push(c);
            Ok(())
        },
    )
    .unwrap();

    let title_for = |id: &str| {
        convs
            .iter()
            .find(|c| c.external_id.as_deref() == Some(id))
            .and_then(|c| c.title.as_deref())
            .map(str::to_string)
    };
    assert_eq!(title_for(sid_db_wins).as_deref(), Some("From database"));
    assert_eq!(title_for(sid_blank_db).as_deref(), Some("From sidecar"));
    assert_eq!(
        title_for(sid_missing_db).as_deref(),
        Some("Renamed in chat")
    );
    assert_eq!(
        title_for(sid_ai_title).as_deref(),
        Some("Generated AI title")
    );
    assert_eq!(title_for(sid_prompt).as_deref(), Some("First prompt 5"));

    // With allow_local_default_stores = false (hermetic): DB MUST NOT be consulted!
    let hermetic_ctx = ctx.without_local_default_stores();
    let mut hermetic_convs = Vec::new();
    let db_for_hermetic = db_path;
    scan_qoder_with_title_resolver(
        &hermetic_ctx,
        &[],
        &mut SourceScanHooks::default(),
        &move |_| Some(db_for_hermetic.clone()),
        &mut |c| {
            hermetic_convs.push(c);
            Ok(())
        },
    )
    .unwrap();
    let hermetic_db_wins = hermetic_convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(sid_db_wins))
        .unwrap();
    assert_eq!(hermetic_db_wins.title.as_deref(), Some("From sidecar"));
}
