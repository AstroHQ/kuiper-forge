defmodule KuiperCoordinator.Forge.RunnerScope do
  @moduledoc """
  Where runners register: an organization, a repository, a Forgejo user (`@name`) or a Forgejo
  instance (`*`). GitHub supports org and repo scopes only.
  """

  @type t ::
          {:org, String.t()} | {:repo, String.t(), String.t()} | {:user, String.t()} | :instance

  @spec parse(String.t()) :: {:ok, t()} | {:error, String.t()}
  # Same rules as Rust's ForgejoForgeConfig::runner_scopes (config.rs): "@", "@me/repo" and
  # "*/repo" are errors, not a strange org, user or repo.
  def parse(scope) when is_binary(scope) do
    case String.split(scope, "/") do
      ["*"] ->
        {:ok, :instance}

      ["@" <> name] when name != "" ->
        {:ok, {:user, name}}

      [org] when org != "" ->
        if String.starts_with?(org, "@"), do: invalid(scope), else: {:ok, {:org, org}}

      [owner, repo] when owner not in ["", "*"] and repo != "" ->
        if String.starts_with?(owner, "@"), do: invalid(scope), else: {:ok, {:repo, owner, repo}}

      _ ->
        invalid(scope)
    end
  end

  defp invalid(scope),
    do: {:error, "invalid runner scope #{inspect(scope)}: use org, owner/repo, @user or *"}

  @doc "The config string for a scope; `parse/1` reads it back."
  @spec to_string(t()) :: String.t()
  def to_string({:org, org}), do: org
  def to_string({:repo, owner, repo}), do: owner <> "/" <> repo
  def to_string({:user, name}), do: "@" <> name
  def to_string(:instance), do: "*"

  @spec github?(t()) :: boolean()
  def github?({:org, _}), do: true
  def github?({:repo, _, _}), do: true
  def github?(_), do: false

  @doc "The GitHub account whose App installation covers this scope."
  def account({:org, name}), do: name
  def account({:repo, owner, _}), do: owner
  def account({:user, name}), do: name
  def account(:instance), do: ""

  def runners_path({:org, org}), do: "/orgs/#{org}/actions/runners"
  def runners_path({:repo, owner, repo}), do: "/repos/#{owner}/#{repo}/actions/runners"

  def registration_token_path(scope), do: runners_path(scope) <> "/registration-token"
  def jitconfig_path(scope), do: runners_path(scope) <> "/generate-jitconfig"
  def runner_path(scope, id), do: runners_path(scope) <> "/#{id}"

  @doc "The URL agents pass to the GitHub runner's config.sh."
  def github_url({:org, org}), do: "https://github.com/#{org}"
  def github_url({:repo, owner, repo}), do: "https://github.com/#{owner}/#{repo}"
end
