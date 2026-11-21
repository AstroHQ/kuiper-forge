defmodule KuiperCoordinator.GitHub.Client.Cache do
  @moduledoc "Installation ids (by lowercased account) and installation tokens for one GitHub App."
  use Agent

  def start_link(opts \\ []) do
    Agent.start_link(fn -> %{installations: %{}, tokens: %{}} end, Keyword.take(opts, [:name]))
  end

  def installation(cache, account), do: Agent.get(cache, &Map.get(&1.installations, account))

  def put_installations(cache, map),
    do: Agent.update(cache, &%{&1 | installations: Map.merge(&1.installations, map)})

  def installations(cache), do: Agent.get(cache, &Map.keys(&1.installations))

  def installation_map(cache), do: Agent.get(cache, & &1.installations)

  def token(cache, installation_id), do: Agent.get(cache, &Map.get(&1.tokens, installation_id))

  def put_token(cache, installation_id, token, expires_at),
    do: Agent.update(cache, &put_in(&1, [:tokens, installation_id], {token, expires_at}))
end
