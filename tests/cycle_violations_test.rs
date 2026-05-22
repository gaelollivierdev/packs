#[allow(deprecated)]
use assert_cmd::cargo::cargo_bin;
use assert_cmd::prelude::*;
use pretty_assertions::assert_eq;
use std::{error::Error, path::Path, process::Command};

mod common;

#[test]
fn test_update_emits_cycle_and_dependency_for_cycle_inducing_implicit_dep(
) -> Result<(), Box<dyn Error>> {
    let package_todo_yml_filepath = Path::new(
        "tests/fixtures/app_with_cycle_violations/packs/baz/package_todo.yml",
    );
    let _ = std::fs::remove_file(package_todo_yml_filepath);

    Command::new(cargo_bin!("packs"))
        .arg("--project-root")
        .arg("tests/fixtures/app_with_cycle_violations")
        .arg("update")
        .assert()
        .success();

    let actual = std::fs::read_to_string(package_todo_yml_filepath)?;
    let expected = String::from(
        "\
# This file contains a list of dependencies that are not part of the long term plan for the
# 'packs/baz' package.
# We should generally work to reduce this list over time.
#
# You can regenerate this file using the following command:
#
# bin/packwerk update-todo
---
packs/foo:
  \"::Foo\":
    violations:
      cycle: packs/baz -> packs/foo -> packs/bar -> packs/baz
      dependency:
    files:
    - packs/baz/app/services/baz.rb
",
    );
    std::fs::remove_file(package_todo_yml_filepath)?;
    assert_eq!(expected, actual);

    common::teardown();

    Ok(())
}

#[test]
fn test_check_does_not_emit_cycle_for_non_cycle_implicit_dep(
) -> Result<(), Box<dyn Error>> {
    // simple_app: foo references bar implicitly, but bar doesn't depend on
    // foo, so the implicit dep doesn't close a cycle. We should see only the
    // dependency violation, not a cycle one.
    let stdout_bytes = Command::new(cargo_bin!("packs"))
        .arg("--project-root")
        .arg("tests/fixtures/simple_app")
        .arg("check")
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let stripped =
        String::from_utf8_lossy(&strip_ansi_escapes::strip(stdout_bytes))
            .to_string();

    assert!(
        stripped.contains("Dependency violation"),
        "expected a dependency violation, got: {}",
        stripped
    );
    assert!(
        !stripped.contains("Cycle violation"),
        "did not expect a cycle violation, got: {}",
        stripped
    );

    common::teardown();
    Ok(())
}
