defmodule KuiperCoordinator.Forge.RunnerScopeTest do
  use ExUnit.Case, async: true

  alias KuiperCoordinator.Forge.RunnerScope

  test "parses org, repo, user and instance scopes" do
    assert RunnerScope.parse("my-org") == {:ok, {:org, "my-org"}}
    assert RunnerScope.parse("owner/repo") == {:ok, {:repo, "owner", "repo"}}
    assert RunnerScope.parse("@jfro") == {:ok, {:user, "jfro"}}
    assert RunnerScope.parse("*") == {:ok, :instance}
    assert {:error, _} = RunnerScope.parse("a/b/c")
    assert {:error, _} = RunnerScope.parse("")
  end

  test "rejects the scopes Rust rejects (config.rs runner_scopes)" do
    for bad <- ["@", "@me/repo", "*/repo", "/repo", "repo/", "@/x"] do
      assert {:error, _} = RunnerScope.parse(bad), "#{inspect(bad)} should be an error"
    end
  end

  test "GitHub API paths for org and repo scopes" do
    org = {:org, "my-org"}
    repo = {:repo, "owner", "repo"}

    assert RunnerScope.registration_token_path(org) ==
             "/orgs/my-org/actions/runners/registration-token"

    assert RunnerScope.registration_token_path(repo) ==
             "/repos/owner/repo/actions/runners/registration-token"

    assert RunnerScope.jitconfig_path(org) == "/orgs/my-org/actions/runners/generate-jitconfig"

    assert RunnerScope.jitconfig_path(repo) ==
             "/repos/owner/repo/actions/runners/generate-jitconfig"

    assert RunnerScope.runners_path(org) == "/orgs/my-org/actions/runners"
    assert RunnerScope.runners_path(repo) == "/repos/owner/repo/actions/runners"
    assert RunnerScope.runner_path(repo, 42) == "/repos/owner/repo/actions/runners/42"
  end

  test "account is the org or the repo owner" do
    assert RunnerScope.account({:org, "MyOrg"}) == "MyOrg"
    assert RunnerScope.account({:repo, "Owner", "Repo"}) == "Owner"
  end

  test "scope URL handed to agents" do
    assert RunnerScope.github_url({:org, "acme"}) == "https://github.com/acme"
    assert RunnerScope.github_url({:repo, "acme", "app"}) == "https://github.com/acme/app"
  end
end
