defmodule KuiperCoordinator.Webhooks.LabelsTest do
  # Ported from kuiper-forge/src/webhook.rs label tests (parity W5, W6)
  use ExUnit.Case, async: true

  alias KuiperCoordinator.Webhooks.Labels

  defp mapping(labels, opts \\ []) do
    %{
      labels: labels,
      runner_scope: opts[:scope],
      runner_group: opts[:group],
      forge: opts[:forge]
    }
  end

  test "required labels: all present, case-insensitive; empty accepts all" do
    required = ["self-hosted"]
    assert Labels.has_required?(["self-hosted", "macOS"], required)
    assert Labels.has_required?(["SELF-HOSTED"], required)
    refute Labels.has_required?(["ubuntu-latest"], required)
    refute Labels.has_required?([], required)
    assert Labels.has_required?(["ubuntu-latest"], [])
    assert Labels.has_required?(["self-hosted", "macOS", "ARM64"], ["self-hosted", "macOS"])
    refute Labels.has_required?(["self-hosted", "Linux"], ["self-hosted", "macOS"])
  end

  test "default required labels are [self-hosted] (#4, e8b974a)" do
    assert Labels.default_required() == ["self-hosted"]
  end

  test "match: superset and case-insensitive, first match wins" do
    mappings = [
      mapping(["self-hosted", "macOS", "ARM64"], scope: {:org, "test-org"}),
      mapping(["self-hosted", "Linux"],
        scope: {:repo, "test-org", "test-repo"},
        group: "linux-runners"
      )
    ]

    assert %{runner_scope: {:org, "test-org"}} =
             Labels.match("github", ["self-hosted", "macOS", "ARM64"], mappings)

    assert Labels.match("github", ["self-hosted", "macOS", "ARM64", "gpu"], mappings)
    assert Labels.match("github", ["SELF-HOSTED", "macos", "arm64"], mappings)
    refute Labels.match("github", ["self-hosted", "macOS"], mappings)
    refute Labels.match("github", ["ubuntu-latest"], mappings)

    assert %{runner_scope: {:repo, _, _}} =
             Labels.match("github", ["self-hosted", "Linux"], mappings)
  end

  test "match: no mappings or no labels never match" do
    refute Labels.match("github", ["self-hosted"], [])
    refute Labels.match("github", [], [mapping(["self-hosted"])])
    # a mapping with no labels matches every job
    assert Labels.match("github", ["anything"], [mapping([], group: "all")])
  end

  test "match respects a mapping's forge" do
    mappings = [
      mapping(["linux"], group: "forgejo", forge: "knope"),
      mapping(["linux"], group: "any")
    ]

    assert %{runner_group: "forgejo"} = Labels.match("knope", ["linux"], mappings)
    assert %{runner_group: "any"} = Labels.match("github", ["linux"], mappings)
    refute Labels.match("github", ["linux"], Enum.take(mappings, 1))
  end

  test "resolve_scope: no mappings -> default; mapping scope wins; group kept; no match -> :ignore" do
    default = {:org, "default-org"}
    assert {:ok, ^default, nil} = Labels.resolve_scope("github", ["self-hosted"], [], default)

    mappings = [
      mapping(["self-hosted", "macOS"], group: "mac"),
      mapping(["self-hosted", "Linux"], scope: {:repo, "o", "r"})
    ]

    assert {:ok, ^default, "mac"} =
             Labels.resolve_scope("github", ["self-hosted", "macOS"], mappings, default)

    assert {:ok, {:repo, "o", "r"}, nil} =
             Labels.resolve_scope("github", ["self-hosted", "Linux"], mappings, default)

    assert :ignore = Labels.resolve_scope("github", ["ubuntu-latest"], mappings, default)
  end
end
