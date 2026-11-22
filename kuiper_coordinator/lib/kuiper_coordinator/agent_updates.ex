defmodule KuiperCoordinator.AgentUpdates do
  @moduledoc """
  Newest agent releases on GitHub, so the admin UI can flag agents running an older version
  (Rust agent_updates.rs, parity U1, #75).

  Each agent crate is released under its own tag, e.g. `kuiper-proxmox-agent-v0.5.0`. This
  server polls the repository's releases every 6 hours (30 minutes after a failure), following
  `Link: rel="next"` up to 50 pages, and keeps the newest non-draft, non-prerelease version of
  each `kuiper-*-agent` crate. A failed page keeps the last good answer. Runs with the admin UI
  unless `KUIPER_UPDATE_CHECK` is 0/false/no/off. Reads go through `:persistent_term`.
  """
  use GenServer
  require Logger

  @releases_url "https://api.github.com/repos/AstroHQ/kuiper-forge/releases?per_page=100"
  @refresh_every :timer.hours(6)
  @retry_after :timer.minutes(30)
  @max_pages 50
  @key {__MODULE__, :latest}

  ## reads

  @doc """
  The newest release when an agent of `agent_type` reporting `version` is behind it, as
  `%{latest:, url:, published_at:}`. nil when current, ahead, unknown, not semver, or nothing
  is known yet.
  """
  def check(agent_type, version) when is_binary(version) do
    with {:ok, running} <- version |> String.trim_leading("v") |> Version.parse(),
         %{version: newest} = r <- Map.get(latest(), "kuiper-#{agent_type}-agent"),
         :gt <- Version.compare(newest, running) do
      %{latest: to_string(newest), url: r.url, published_at: r.published_at}
    else
      _ -> nil
    end
  end

  def check(_agent_type, _version), do: nil

  defp latest, do: :persistent_term.get(@key, %{})

  @doc false
  def set(releases), do: :persistent_term.put(@key, releases)

  ## config

  @doc "Whether the check runs: on unless the value says 0/false/no/off."
  def enabled_from_env(nil), do: true

  def enabled_from_env(v),
    do: String.downcase(String.trim(v)) not in ~w(0 false no off)

  @doc "A child spec when the check is on (KUIPER_UPDATE_CHECK), else nil."
  def child do
    admin = Application.get_env(:kuiper_coordinator, :admin, [])

    if Keyword.get(admin, :update_check, false) do
      __MODULE__
    else
      Logger.info("Agent update check is off")
      nil
    end
  end

  ## fetching

  @doc """
  Fetches every page from `url` and replaces what's known. When any page fails, what's known
  stays as it was, so a partial list never replaces a full one.
  """
  def refresh(url \\ @releases_url, req_options \\ []) do
    started = System.monotonic_time(:millisecond)

    case fetch(url, req_options, 1, []) do
      {:ok, releases} ->
        newest = newest_agent_releases(releases)
        set(newest)

        Logger.info(
          "Checked GitHub for agent releases in #{System.monotonic_time(:millisecond) - started}ms: " <>
            Enum.map_join(newest, ", ", fn {name, r} -> "#{name} #{r.version}" end)
        )

        :ok

      {:error, _} = error ->
        error
    end
  end

  defp fetch(url, _opts, page, acc) when page > @max_pages do
    Logger.warning(
      "Stopped following GitHub release pages at #{@max_pages} (next #{url}, #{length(acc)} fetched); older releases are ignored"
    )

    {:ok, acc}
  end

  defp fetch(url, opts, page, acc) do
    case fetch_page(url, opts) do
      {:ok, batch, nil} -> {:ok, acc ++ batch}
      {:ok, batch, next} -> fetch(next, opts, page + 1, acc ++ batch)
      {:error, reason} -> {:error, "page #{page} (#{url}): #{reason}"}
    end
  end

  defp fetch_page(url, opts) do
    req =
      Req.new(
        url: url,
        headers: [{"accept", "application/vnd.github+json"}],
        user_agent: "kuiper-coordinator",
        receive_timeout: 30_000,
        retry: false
      )
      |> Req.merge(opts)

    case Req.get(req) do
      {:ok, %{status: status, body: body} = resp} when status in 200..299 and is_list(body) ->
        next = resp |> Req.Response.get_header("link") |> Enum.find_value(&next_link/1)
        {:ok, body, next}

      {:ok, %{status: status, body: body}} when status in 200..299 ->
        {:error, "not a release list (#{status}): #{snippet(body)}"}

      {:ok, %{status: status, body: body}} ->
        {:error, "GitHub returned #{status}: #{snippet(body)}"}

      {:error, e} ->
        {:error, Exception.message(e)}
    end
  end

  defp snippet(body) when is_binary(body), do: String.slice(body, 0, 200)
  defp snippet(body), do: body |> inspect() |> String.slice(0, 200)

  @doc ~S|The `rel="next"` URL from a `Link` header, or nil.|
  def next_link(header) do
    header
    |> String.split(",")
    |> Enum.find_value(fn link ->
      case String.split(link, ";") do
        [target | params] ->
          if Enum.any?(params, &(String.trim(&1) in [~s(rel="next"), "rel=next"])),
            do: target |> String.trim() |> String.trim_leading("<") |> String.trim_trailing(">")

        _ ->
          nil
      end
    end)
  end

  @doc "Newest non-draft, non-prerelease version of each `kuiper-*-agent` crate."
  def newest_agent_releases(releases) do
    Enum.reduce(releases, %{}, fn r, acc ->
      with false <- r["draft"] == true or r["prerelease"] == true,
           tag when is_binary(tag) <- r["tag_name"],
           [name, v] <- split_tag(tag),
           true <- String.starts_with?(name, "kuiper-") and String.ends_with?(name, "-agent"),
           {:ok, %Version{pre: []} = version} <- Version.parse(v),
           true <- newer?(version, acc[name]) do
        Map.put(acc, name, %{
          version: version,
          url: r["html_url"],
          published_at: published_at(r["published_at"])
        })
      else
        _ -> acc
      end
    end)
  end

  defp newer?(_version, nil), do: true
  defp newer?(version, %{version: known}), do: Version.compare(version, known) == :gt

  # rsplit on "-v"
  defp split_tag(tag) do
    case :binary.matches(tag, "-v") do
      [] ->
        nil

      matches ->
        {pos, _} = List.last(matches)
        [binary_part(tag, 0, pos), binary_part(tag, pos + 2, byte_size(tag) - pos - 2)]
    end
  end

  defp published_at(s) when is_binary(s) do
    case DateTime.from_iso8601(s) do
      {:ok, at, _} -> at
      _ -> nil
    end
  end

  defp published_at(_), do: nil

  ## server

  def start_link(opts), do: GenServer.start_link(__MODULE__, opts, name: __MODULE__)

  @impl true
  def init(opts), do: {:ok, Map.new(opts), {:continue, :refresh}}

  @impl true
  def handle_continue(:refresh, state), do: {:noreply, do_refresh(state)}

  @impl true
  def handle_info(:refresh, state), do: {:noreply, do_refresh(state)}

  defp do_refresh(state) do
    wait =
      case refresh(Map.get(state, :url, @releases_url), Map.get(state, :req_options, [])) do
        :ok ->
          @refresh_every

        {:error, reason} ->
          Logger.warning("Agent update check failed: #{reason}; retrying in 30 minutes")
          @retry_after
      end

    Process.send_after(self(), :refresh, wait)
    state
  end
end
