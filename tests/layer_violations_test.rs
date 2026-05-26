#[allow(deprecated)]
use assert_cmd::cargo::cargo_bin;
use assert_cmd::prelude::*;
use pretty_assertions::assert_eq;
use std::{error::Error, path::Path, process::Command};

mod common;
#[test]
fn test_check() -> Result<(), Box<dyn Error>> {
    let output = Command::new(cargo_bin!("packs"))
        .arg("--project-root")
        .arg("tests/fixtures/layer_violations")
        .arg("--debug")
        .arg("check")
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let stripped_output =
        String::from_utf8_lossy(&strip_ansi_escapes::strip(output)).to_string();

    assert!(stripped_output.contains("1 violation(s) detected:"));
    assert!(stripped_output.contains("packs/feature_flags/app/services/feature_flags.rb:2:0\nLayer violation: `::Payments` belongs to `packs/payments` (whose layer is `product`) cannot be accessed from `packs/feature_flags` (whose layer is `utilities`)"));

    common::teardown();
    Ok(())
}

#[test]
fn test_check_enforce_layers_disabled() -> Result<(), Box<dyn Error>> {
    Command::new(cargo_bin!("packs"))
        .arg("--project-root")
        .arg("tests/fixtures/layer_violations")
        .arg("--debug")
        .arg("--disable-enforce-layers")
        .arg("check")
        .assert()
        .success();

    common::teardown();
    Ok(())
}

#[test]
fn test_update_emits_layer_detail_when_detailed_violations_enabled(
) -> Result<(), Box<dyn Error>> {
    // The fixture has `detailed_violations: true` in packwerk.yml; running
    // `pks update` should record the layer violation in the new map form
    // with a `<referencing_layer> < <defining_layer>` detail string.
    let package_todo_yml_filepath = Path::new(
        "tests/fixtures/app_with_detailed_layer_violations/packs/feature_flags/package_todo.yml",
    );
    let _ = std::fs::remove_file(package_todo_yml_filepath);

    Command::new(cargo_bin!("packs"))
        .arg("--project-root")
        .arg("tests/fixtures/app_with_detailed_layer_violations")
        .arg("update")
        .assert()
        .success();

    let actual = std::fs::read_to_string(package_todo_yml_filepath)?;
    let expected = String::from(
        "\
# This file contains a list of dependencies that are not part of the long term plan for the
# 'packs/feature_flags' package.
# We should generally work to reduce this list over time.
#
# You can regenerate this file using the following command:
#
# bin/packwerk update-todo
---
packs/payments:
  \"::Payments\":
    violations:
      layer: utilities < product
    files:
    - packs/feature_flags/app/services/feature_flags.rb
",
    );
    std::fs::remove_file(package_todo_yml_filepath)?;
    assert_eq!(expected, actual);

    common::teardown();
    Ok(())
}
