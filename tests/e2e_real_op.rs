use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn run_checked(cmd: &mut Command, context: &str) -> String {
    eprintln!("[e2e] {context}: {:?}", cmd);
    let out = cmd.output().expect("failed to execute command");
    if !out.status.success() {
        panic!(
            "{context} failed\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    eprintln!("[e2e] {context}: ok");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn ensure_exists(path: &Path, context: &str) {
    assert!(path.exists(), "{context}: {} does not exist", path.display());
}

#[test]
fn e2e_real_op_create_run_shorthand_gen_delete() {
    if std::env::var("OPZ_E2E").ok().as_deref() != Some("1") {
        eprintln!("skip e2e: set OPZ_E2E=1 to run this test");
        return;
    }

    let opz_bin = env!("CARGO_BIN_EXE_opz");
    let temp = tempfile::tempdir().expect("create tempdir");
    let env1 = temp.path().join(".env");
    let env2 = temp.path().join(".env2");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before UNIX_EPOCH")
        .as_millis();
    let pid = std::process::id();
    let item_title = format!("opz-e2e-{now}-{pid}");

    let foo = format!("foo_{now}_{pid}");
    let bar = format!("bar_{now}_{pid}");
    let env_body = format!("E2E_OPZ_FOO={foo}\nE2E_OPZ_BAR={bar}\n");
    eprintln!("[e2e] step1: write {}", env1.display());
    fs::write(&env1, env_body).expect("write .env");
    ensure_exists(&env1, "step1");

    eprintln!("[e2e] step2: create item '{item_title}'");
    run_checked(
        Command::new(opz_bin)
            .current_dir(temp.path())
            .arg("create")
            .arg(&item_title)
            .arg(&env1),
        "step2 create",
    );

    // Step3a: run subcommand
    eprintln!("[e2e] step3a: run subcommand");
    run_checked(
        Command::new(opz_bin)
            .current_dir(temp.path())
            .arg("run")
            .arg(&item_title)
            .arg("--")
            .arg("sh")
            .arg("-c")
            .arg("test \"$E2E_OPZ_FOO\" = \"$1\" && test \"$E2E_OPZ_BAR\" = \"$2\"")
            .arg("x")
            .arg(&foo)
            .arg(&bar),
        "step3a run subcommand",
    );

    // Step3b: top-level shorthand (without explicit run)
    eprintln!("[e2e] step3b: run shorthand");
    run_checked(
        Command::new(opz_bin)
            .current_dir(temp.path())
            .arg(&item_title)
            .arg("--")
            .arg("sh")
            .arg("-c")
            .arg("test \"$E2E_OPZ_FOO\" = \"$1\" && test \"$E2E_OPZ_BAR\" = \"$2\"")
            .arg("x")
            .arg(&foo)
            .arg(&bar),
        "step3b shorthand",
    );

    eprintln!("[e2e] step4: gen {}", env2.display());
    run_checked(
        Command::new(opz_bin)
            .current_dir(temp.path())
            .arg("gen")
            .arg("--env-file")
            .arg(&env2)
            .arg(&item_title),
        "step4 gen",
    );
    ensure_exists(&env2, "step4");

    let generated = fs::read_to_string(&env2).expect("read .env2");
    assert!(
        generated.contains("E2E_OPZ_FOO=op://"),
        "step4: expected E2E_OPZ_FOO op:// reference in .env2\ncontent:\n{generated}"
    );
    assert!(
        generated.contains("E2E_OPZ_BAR=op://"),
        "step4: expected E2E_OPZ_BAR op:// reference in .env2\ncontent:\n{generated}"
    );

    eprintln!("[e2e] step5: delete item '{item_title}'");
    run_checked(
        Command::new("op").arg("item").arg("delete").arg(&item_title),
        "step5 op item delete",
    );
    eprintln!("[e2e] done");
}
