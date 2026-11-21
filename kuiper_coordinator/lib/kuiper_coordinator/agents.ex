defmodule KuiperCoordinator.Agents do
  @moduledoc """
  Registered agents, registration tokens and uploaded agent logs (Rust `auth.rs`, `agent_logs.rs`).
  """
  import Ecto.Query

  alias KuiperCoordinator.{AgentRegistry, CA, Repo}
  alias KuiperCoordinator.Agents.{Agent, Bundle, LogLine, RegistrationToken}

  @max_log_batch 2_000
  @max_message_bytes 16 * 1024
  @max_target_bytes 256
  @revoked_retention_days 7

  def max_log_batch, do: @max_log_batch

  ## Registration tokens

  @doc "A new single-use token, `reg_` + 32 alphanumerics, valid for `ttl_seconds` (proto default 1 h)."
  def create_registration_token(ttl_seconds \\ 3600, created_by \\ "admin") do
    token = %RegistrationToken{
      token: "reg_" <> CA.random_string(32),
      expires_at: DateTime.add(DateTime.utc_now(), ttl_seconds, :second),
      created_by: created_by
    }

    Repo.insert(token)
  end

  @doc """
  A registration bundle for `token` (what an operator pastes into an agent's config). It carries
  the CA agents use to verify the coordinator before registering, the same one RegisterResponse
  gives (`AgentGrpc.TLS.server_ca_pem/1`), and none only in chain mode with native roots.
  """
  def registration_bundle(
        %RegistrationToken{token: token},
        coordinator_url,
        settings \\ KuiperCoordinator.AgentGrpc.settings()
      ) do
    trust_mode = Keyword.get(settings, :server_trust_mode, "ca")

    ca_pem =
      if settings[:ca_cert],
        do: KuiperCoordinator.AgentGrpc.TLS.server_ca_pem(settings),
        else: CA.pem()

    Bundle.encode(token, if(ca_pem == "", do: nil, else: ca_pem), trust_mode, coordinator_url)
  end

  @doc "agent_type rule (agent_registry.rs:45-58): 1-32 of `[a-z0-9-]` after lowercasing."
  def parse_agent_type(type) when is_binary(type) do
    name = String.downcase(type)

    if name =~ ~r/\A[a-z0-9-]{1,32}\z/ do
      {:ok, name}
    else
      {:error, "Invalid agent type #{inspect(type)}: up to 32 letters, digits and '-'"}
    end
  end

  @doc """
  Exchanges a registration token for a client cert. The agent type is checked before the token is
  touched, so a bad type doesn't burn it (A10). The token is consumed atomically before the expiry
  check (A11): of two concurrent calls with one token, exactly one gets it, and an expired token is
  burned.
  """
  def register(token, hostname, agent_type) do
    with {:type, {:ok, type}} <- {:type, parse_agent_type(agent_type)},
         {:ok, result} <- Repo.transaction(fn -> consume_and_issue(token, type, hostname) end) do
      result
    else
      {:type, {:error, msg}} -> {:error, :invalid_argument, msg}
    end
  end

  # One transaction: if issuing or the agent insert fails, the token comes back. An invalid or
  # expired token returns an error without raising, so an expired one stays burned (A11).
  defp consume_and_issue(token, type, hostname) do
    with {:token, {:ok, reg}} <- {:token, consume_token(token)},
         {:expiry, :ok} <- {:expiry, check_expiry(reg)} do
      agent_id = CA.new_agent_id(type, hostname)
      issued = CA.issue_agent_cert(agent_id)

      %Agent{
        agent_id: agent_id,
        hostname: hostname,
        agent_type: type,
        labels: [],
        max_vms: 0,
        serial_number: issued.serial,
        expires_at: issued.expires_at
      }
      |> Repo.insert!()

      {:ok, Map.put(issued, :agent_id, agent_id)}
    else
      {:token, :error} -> {:error, :unauthenticated, "Invalid or already used registration token"}
      {:expiry, :error} -> {:error, :unauthenticated, "Registration token has expired"}
    end
  end

  @doc "Every registration token, oldest first, in plaintext (management.rs ListTokens, M2)."
  def list_registration_tokens do
    Repo.all(from(t in RegistrationToken, order_by: [asc: t.inserted_at, asc: t.token]))
  end

  @doc "Deletes a registration token; true when one was there."
  def delete_registration_token(token) when is_binary(token) do
    {count, _} = Repo.delete_all(from(t in RegistrationToken, where: t.token == ^token))
    count == 1
  end

  defp consume_token(token) do
    case Repo.delete_all(from(t in RegistrationToken, where: t.token == ^token, select: t)) do
      {1, [reg]} -> {:ok, reg}
      _ -> :error
    end
  end

  defp check_expiry(%RegistrationToken{expires_at: at}) do
    if DateTime.compare(at, DateTime.utc_now()) == :lt, do: :error, else: :ok
  end

  ## Agents

  def get_agent(agent_id), do: Repo.get(Agent, agent_id)

  @doc "Every registered agent, revoked ones too, newest first (auth.rs list_agents)."
  def list_agents do
    Repo.all(from(a in Agent, order_by: [desc: a.inserted_at, asc: a.agent_id]))
  end

  @doc "Exists, not revoked, cert not expired (auth.rs is_agent_valid)."
  def valid_agent?(agent_id) when is_binary(agent_id) do
    now = DateTime.utc_now()

    Repo.exists?(
      from(a in Agent,
        where: a.agent_id == ^agent_id and is_nil(a.revoked_at) and a.expires_at > ^now
      )
    )
  end

  def valid_agent?(_), do: false

  @doc """
  Syncs what the agent reports (labels, max_vms, version) and nothing else, so a revoke that
  lands at the same time isn't undone (A14). Writes only when something changed.
  """
  def sync_metadata(agent_id, labels, max_vms, agent_version) do
    version = if agent_version in [nil, ""], do: nil, else: agent_version

    {count, _} =
      from(a in Agent,
        where: a.agent_id == ^agent_id,
        where:
          a.labels != ^labels or a.max_vms != ^max_vms or
            fragment("? IS DISTINCT FROM ?", a.agent_version, ^version),
        update: [
          set: [
            labels: ^labels,
            max_vms: ^max_vms,
            agent_version: ^version,
            updated_at: ^DateTime.utc_now()
          ]
        ]
      )
      |> Repo.update_all([])

    count
  end

  @doc """
  Revokes an agent and closes its live AgentStream now (D6; the Rust coordinator leaves it open).
  """
  def revoke_agent(agent_id) do
    {count, _} =
      from(a in Agent, where: a.agent_id == ^agent_id and is_nil(a.revoked_at))
      |> Repo.update_all(set: [revoked_at: DateTime.utc_now()])

    AgentRegistry.disconnect(agent_id, :revoked)
    if count == 1, do: :ok, else: {:error, :not_found}
  end

  @doc """
  Marks an agent unavailable (no new runners; running ones finish) or available again
  (Rust set_agent_unavailable, parity #40). The DB is written first, then `{:agent_availability,
  agent_id, available?}` goes out on PubSub "agents" so scheduling follows.
  """
  def set_unavailable(agent_id, unavailable?) when is_boolean(unavailable?) do
    value = if unavailable?, do: DateTime.utc_now()

    {count, _} =
      from(a in Agent, where: a.agent_id == ^agent_id)
      |> Repo.update_all(set: [unavailable_at: value, updated_at: DateTime.utc_now()])

    if count == 1 do
      AgentRegistry.broadcast({:agent_availability, agent_id, not unavailable?})
      :ok
    else
      {:error, :not_found}
    end
  end

  @doc "Hourly housekeeping (A13): drop expired tokens and agents revoked more than 7 days ago."
  def purge_expired do
    now = DateTime.utc_now()
    cutoff = DateTime.add(now, -@revoked_retention_days, :day)
    {tokens, _} = Repo.delete_all(from(t in RegistrationToken, where: t.expires_at < ^now))

    {agents, _} =
      Repo.delete_all(
        from(a in Agent, where: not is_nil(a.revoked_at) and a.revoked_at < ^cutoff)
      )

    %{tokens: tokens, agents: agents}
  end

  ## Logs

  @log_retention_days 7

  @doc """
  Stores an uploaded batch (agent_logs.rs insert_batch): a WARN marker just before the first line
  when the agent dropped lines, unknown levels as INFO, target and message capped at a UTF-8
  boundary.
  """
  def insert_log_batch(agent_id, records, dropped) do
    marker =
      if dropped > 0 do
        ts =
          case records do
            [first | _] -> first.timestamp_micros - 1
            [] -> System.os_time(:microsecond)
          end

        [
          %{
            agent_id: agent_id,
            ts: ts,
            seq: 0,
            level: 2,
            target: "kuiper_coordinator",
            message: "agent dropped #{dropped} log line(s) because its upload buffer was full"
          }
        ]
      else
        []
      end

    rows =
      marker ++
        Enum.map(records, fn r ->
          %{
            agent_id: agent_id,
            ts: r.timestamp_micros,
            seq: r.seq,
            level: level(r.level),
            target: truncate(r.target, @max_target_bytes),
            message: truncate(r.message, @max_message_bytes)
          }
        end)

    # one transaction, so a failed chunk leaves nothing behind, and (agent_id, ts, seq) is unique,
    # so a retried batch that did land adds nothing (D25);
    # a per-agent lock so concurrent uploads can't all pass the cap with the same cutoff
    {:ok, :ok} =
      Repo.transaction(fn ->
        Repo.query!("SELECT pg_advisory_xact_lock(hashtext($1))", [agent_id])

        rows
        |> Enum.chunk_every(1_000)
        |> Enum.each(
          &Repo.insert_all(LogLine, &1,
            on_conflict: :nothing,
            conflict_target: [:agent_id, :ts, :seq]
          )
        )

        cap_logs(agent_id)
        :ok
      end)

    :ok
  end

  @doc """
  Newest log lines kept per agent (agent_logs.rs MAX_LINES_PER_AGENT), so a chatty or
  compromised agent can't fill the database. `config :kuiper_coordinator, :agent_log_max_lines`.
  """
  def max_log_lines, do: Application.get_env(:kuiper_coordinator, :agent_log_max_lines, 100_000)

  @doc """
  Trims `agent_id` to its newest `max` lines, by insertion order (the row id), not the
  agent-supplied timestamps. Returns how many lines went.
  """
  def cap_logs(agent_id, max \\ max_log_lines()) do
    first_over =
      Repo.one(
        from(l in LogLine,
          where: l.agent_id == ^agent_id,
          order_by: [desc: l.id],
          offset: ^max,
          limit: 1,
          select: l.id
        )
      )

    case first_over do
      nil ->
        0

      id ->
        {n, _} =
          Repo.delete_all(from(l in LogLine, where: l.agent_id == ^agent_id and l.id <= ^id))

        n
    end
  end

  @doc """
  Hourly log housekeeping (agent_logs.rs prune): lines stored more than #{@log_retention_days}
  days ago go (by the server's insert time, not the agent's timestamps), then each agent is
  trimmed to `max_log_lines/0`.
  """
  def prune_logs do
    cutoff = DateTime.add(DateTime.utc_now(), -@log_retention_days, :day)
    {old, _} = Repo.delete_all(from(l in LogLine, where: l.inserted_at < ^cutoff))

    capped =
      from(l in LogLine, distinct: true, select: l.agent_id)
      |> Repo.all()
      |> Enum.reduce(0, &(cap_logs(&1) + &2))

    %{old: old, capped: capped}
  end

  def level(name) do
    case String.upcase(name || "") do
      "ERROR" -> 1
      "WARN" -> 2
      "INFO" -> 3
      "DEBUG" -> 4
      "TRACE" -> 5
      _ -> 3
    end
  end

  @doc "Cuts `s` to at most `max` bytes on a UTF-8 boundary and marks it truncated."
  def truncate(s, max) when byte_size(s) <= max, do: s

  def truncate(s, max) do
    cut = utf8_boundary(s, max)
    binary_part(s, 0, cut) <> " …[truncated]"
  end

  # a continuation byte is 0b10xxxxxx; back up until the cut doesn't split a character
  defp utf8_boundary(_s, 0), do: 0

  defp utf8_boundary(s, cut) do
    case :binary.at(s, cut) do
      b when Bitwise.band(b, 0xC0) == 0x80 -> utf8_boundary(s, cut - 1)
      _ -> cut
    end
  end

  @doc """
  An agent's log lines at `min_level` (1 = error .. 5 = trace) or more severe, newest first,
  older than the `before` cursor (`"<ts>_<seq>"`, as `log_cursor/1` gives; a bad one is ignored),
  at most `limit` (agent_logs.rs recent).
  """
  def recent_logs(agent_id, min_level, before, limit) do
    {ts, seq} = parse_log_cursor(before) || {nil, nil}

    from(l in LogLine,
      where: l.agent_id == ^agent_id and l.level <= ^min_level,
      order_by: [desc: l.ts, desc: l.seq],
      limit: ^limit
    )
    |> then(fn q ->
      if ts, do: where(q, [l], l.ts < ^ts or (l.ts == ^ts and l.seq < ^seq)), else: q
    end)
    |> Repo.all()
  end

  def log_cursor(%LogLine{ts: ts, seq: seq}), do: "#{ts}_#{seq}"

  def parse_log_cursor(cursor) when is_binary(cursor) do
    with [ts, seq] <- String.split(cursor, "_", parts: 2),
         {ts, ""} <- Integer.parse(ts),
         {seq, ""} <- Integer.parse(seq) do
      {ts, seq}
    else
      _ -> nil
    end
  end

  def parse_log_cursor(_), do: nil

  @doc "Minimum level from a filter name (agent_log_routes.rs level_filter): unknown is trace."
  def level_filter(name) do
    case String.downcase(name || "") do
      "error" -> 1
      "warn" -> 2
      "info" -> 3
      "debug" -> 4
      _ -> 5
    end
  end

  def level_name(1), do: "error"
  def level_name(2), do: "warn"
  def level_name(3), do: "info"
  def level_name(4), do: "debug"
  def level_name(_), do: "trace"

  def log_lines(agent_id) do
    Repo.all(
      from(l in LogLine, where: l.agent_id == ^agent_id, order_by: [asc: l.ts, asc: l.seq])
    )
  end
end
