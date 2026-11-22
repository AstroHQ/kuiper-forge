defmodule KuiperCoordinator.Fleet.Forges do
  @moduledoc """
  Builds the configured forges at startup and validates them (Rust main.rs: forges from
  config, then `validate()`, fatal; parity C2, C3, C10, G1, FJ7).

  A spec is a map or keyword list, in the shape of Rust's `[[forges]]`:

    * `%{name: "github", type: :github, app_id: 123, private_key_path: "...", scan_lookback_hours: 168}`
      (or `private_key_env` / `private_key`, a PEM)
    * `%{name: "forgejo", type: :forgejo, url: "https://...", token_env: "FORGEJO_TOKEN",
      scopes: ["acme", "owner/repo", "@user", "*"]}` (or `token_path` / `token`)

  Names are unique and have no `:` (they prefix job keys). Each client's caches are started
  linked to the caller, so they live and die with the fleet manager that owns them.
  """
  alias KuiperCoordinator.Forgejo.Client, as: Forgejo
  alias KuiperCoordinator.GitHub.Client, as: GitHub
  alias KuiperCoordinator.GitHub.Client.Cache, as: GitHubCache

  @type spec :: map() | keyword()

  @doc """
  `[spec]` -> `{:ok, [{name, Forge.t()}]}` in config order, or `{:error, message}`.
  Options: `:validate` (default true), `:req_options` (passed to each client, for tests).
  """
  @spec build([spec()], keyword()) ::
          {:ok, [{String.t(), KuiperCoordinator.Forge.t()}]} | {:error, String.t()}
  def build(specs, opts \\ [])

  def build([], _opts), do: {:error, "no forges configured: add a GitHub or Forgejo forge"}

  def build(specs, opts) when is_list(specs) do
    specs = Enum.map(specs, &Map.new/1)

    with :ok <- check_names(specs) do
      Enum.reduce_while(specs, {:ok, []}, fn spec, {:ok, acc} ->
        with {:ok, forge} <- new(spec, opts),
             :ok <- maybe_validate(spec.name, forge, Keyword.get(opts, :validate, true)) do
          {:cont, {:ok, acc ++ [{spec.name, forge}]}}
        else
          {:error, msg} -> {:halt, {:error, "forge '#{spec.name}': #{msg}"}}
        end
      end)
    end
  end

  @doc """
  Fills in the webhook config's `:sources` (`%{forge: name, secret: secret}`, one per forge in
  webhook mode) from the built forges: each gets its `:source` (`:github` or `:forgejo`) and,
  for Forgejo, the forge's runner `:scopes`. A source that names no built forge is an error.
  """
  @spec webhook_config([{String.t(), KuiperCoordinator.Forge.t()}], keyword()) ::
          {:ok, keyword()} | {:error, String.t()}
  def webhook_config(forges, webhook) do
    sources = Keyword.get(webhook, :sources, [])

    case Enum.find(sources, &(not List.keymember?(forges, &1.forge, 0))) do
      nil ->
        filled =
          Enum.map(sources, fn %{forge: name} = source ->
            case List.keyfind(forges, name, 0) do
              {_, {Forgejo, client}} ->
                Map.merge(source, %{source: :forgejo, scopes: client.scopes})

              _ ->
                Map.put(source, :source, :github)
            end
          end)

        {:ok, Keyword.put(webhook, :sources, filled)}

      %{forge: name} ->
        {:error, "webhook forge '#{name}' isn't a configured forge"}
    end
  end

  defp check_names(specs) do
    names = Enum.map(specs, &Map.get(&1, :name))

    cond do
      bad = Enum.find(names, &(not is_binary(&1) or &1 == "")) ->
        {:error, "every forge needs a name (got #{inspect(bad)})"}

      bad = Enum.find(names, &String.contains?(&1, ":")) ->
        {:error, "forge name '#{bad}' can't contain ':'"}

      length(Enum.uniq(names)) != length(names) ->
        {:error, "forge names must be unique (got #{inspect(names)})"}

      true ->
        :ok
    end
  end

  defp new(%{type: type} = spec, opts) when type in [:forgejo, "forgejo"] do
    {:ok, cache} = Agent.start_link(fn -> %{} end)

    spec
    |> Map.take([:url, :token, :token_env, :token_path, :scopes])
    |> Map.to_list()
    |> Keyword.merge(cache: cache, req_options: Keyword.get(opts, :req_options, []))
    |> Forgejo.new()
    |> case do
      {:ok, c} -> {:ok, {Forgejo, c}}
      err -> err
    end
  end

  defp new(%{type: type} = spec, opts) when type in [:github, "github"] do
    with {:ok, app_id} <- app_id(spec[:app_id]),
         {:ok, pem} <- private_key(spec) do
      {:ok, cache} = GitHubCache.start_link()

      client =
        GitHub.new(
          [
            app_id: app_id,
            private_key: pem,
            cache: cache,
            req_options: Keyword.get(opts, :req_options, [])
          ] ++ Enum.filter([scan_lookback_hours: spec[:scan_lookback_hours]], &elem(&1, 1))
        )

      {:ok, {GitHub, client}}
    end
  end

  defp new(spec, _opts),
    do: {:error, "unknown forge type #{inspect(spec[:type])} (github or forgejo)"}

  defp app_id(id) when is_integer(id) and id > 0, do: {:ok, id}

  defp app_id(id) when is_binary(id) do
    case Integer.parse(id) do
      {n, ""} when n > 0 -> {:ok, n}
      _ -> app_id(nil)
    end
  end

  defp app_id(id), do: {:error, "a GitHub forge needs a numeric app_id (got #{inspect(id)})"}

  defp private_key(spec) do
    # an inline key wins over a key file, as in Rust (config.rs private_key_content): the
    # container case sets KUIPER_GITHUB__PRIVATE_KEY while the TOML still names a path
    pem =
      cond do
        pem = spec[:private_key] ->
          pem

        env = spec[:private_key_env] ->
          System.get_env(env)

        path = spec[:private_key_path] ->
          path |> File.read() |> then(&if(elem(&1, 0) == :ok, do: elem(&1, 1)))

        true ->
          nil
      end

    if is_binary(pem) and String.trim(pem) != "",
      do: {:ok, pem},
      else:
        {:error,
         "a GitHub forge needs a private key (private_key_env, private_key_path or private_key)"}
  end

  defp maybe_validate(_name, _forge, false), do: :ok

  defp maybe_validate(_name, {Forgejo, c}, true), do: Forgejo.validate(c)

  defp maybe_validate(_name, {GitHub, c}, true) do
    case GitHub.validate(c) do
      {:ok, _accounts} -> :ok
      {:error, :no_installations} -> {:error, "the GitHub App has no installations"}
      {:error, e} when is_binary(e) -> {:error, e}
      {:error, e} -> {:error, inspect(e)}
    end
  end
end
