defmodule KuiperCoordinator.RustImport do
  @moduledoc """
  Brings a Rust coordinator's identities and config into Phoenix, so switching over needs no
  re-registration (`mix kuiper.import_rust`, `KuiperCoordinator.Release.import_rust/2`).

  From the Rust database: registered agents (revocations kept, so existing certs from the shared
  CA keep working and revoked ones stay revoked), unexpired registration tokens, admin users
  (their Argon2id hashes verify as they are) and API tokens (the same SHA-256). From config.toml,
  if given: forges with their secrets, provisioning settings and label mappings
  (`KuiperCoordinator.RustImport.Config`). Short-lived state (runners, pending jobs, sessions,
  logs, failures, run history) is left behind (D71).

  It can be rerun: rows are matched by natural key (agent id, token, username, token hash, forge
  name). A row Phoenix already has is left as it is; if it differs it's reported as a conflict.
  The one change a rerun makes to an existing row is a revocation done on the Rust side since.
  What an import brought over is kept in `rust_imports` (as hashes), so a rerun never brings
  back what Phoenix removed since: a used token, a deleted user or forge, a purged agent.

  Nothing in the report is a secret: tokens show as their prefix.
  """
  import Ecto.Query
  require Logger

  alias KuiperCoordinator.{Agents, Forges, Repo}
  alias KuiperCoordinator.Admin.{ApiToken, User}
  alias KuiperCoordinator.Agents.{Agent, RegistrationToken}
  alias KuiperCoordinator.Forges.{Forge, Settings}
  alias KuiperCoordinator.RustImport.{Config, Source}

  @kinds [
    agents: "agents",
    registration_tokens: "registration tokens",
    admin_users: "admin users",
    api_tokens: "api tokens",
    forges: "forges",
    settings: "provisioning settings",
    label_mappings: "label mappings"
  ]

  @type item :: %{
          kind: atom(),
          label: String.t(),
          action: :new | :revoked | :unchanged | :skipped | :conflict,
          note: String.t() | nil,
          apply: (-> any()) | nil,
          ledger: String.t() | nil
        }

  @doc """
  Runs the import. Options: `:from` (the Rust database URL, required), `:config` (config.toml),
  `:dry_run`. Returns `{:ok, report}` (see `format_report/1`) or `{:error, message}`.
  """
  @spec run(keyword()) :: {:ok, map()} | {:error, String.t()}
  def run(opts) do
    from = opts[:from]
    dry_run = opts[:dry_run] == true

    with {:from, from} when is_binary(from) and from != "" <- {:from, from},
         {:ok, config} <- load_config(opts[:config]),
         {:ok, source} <- Source.open(from) do
      try do
        with {:ok, items} <- plan(source, config) do
          report = %{
            from: Source.describe(from),
            config: opts[:config],
            dry_run: dry_run,
            items: items
          }

          if dry_run, do: {:ok, report}, else: with(:ok <- apply_items(items), do: {:ok, report})
        end
      after
        Source.close(source)
      end
    else
      {:from, _} ->
        {:error,
         "give the Rust coordinator's database with --from (sqlite://PATH or postgres://...)"}

      {:error, msg} ->
        {:error, msg}
    end
  end

  @doc """
  Runs the import for the CLI (the mix task, the release): starts what it needs that a bare
  repo doesn't (the vault for forge secrets, PubSub), prints the report, and calls `fail` with
  a message on an error. Returns the report; `System.halt(2)` style exits are the caller's.
  """
  def cli(opts, fail) do
    {:ok, _} = Application.ensure_all_started(:phoenix_pubsub)

    for child <- [KuiperCoordinator.Vault, {Phoenix.PubSub, name: KuiperCoordinator.PubSub}] do
      case Supervisor.start_child(cli_sup(), child) do
        {:ok, _} -> :ok
        {:error, {:already_started, _}} -> :ok
        {:error, {{:already_started, _}, _}} -> :ok
        {:error, :already_present} -> :ok
      end
    end

    # Ecto's debug query log shows a forge's secrets before they're encrypted: keep it quiet
    level = Logger.level()
    if Logger.compare_levels(level, :info) == :lt, do: Logger.configure(level: :info)

    try do
      run(opts)
    after
      Logger.configure(level: level)
    end
    |> case do
      {:ok, report} ->
        IO.write(format_report(report))
        report

      {:error, msg} ->
        fail.(msg)
    end
  end

  defp cli_sup do
    case Process.whereis(__MODULE__.CliSup) do
      nil ->
        {:ok, pid} = Supervisor.start_link([], strategy: :one_for_one, name: __MODULE__.CliSup)
        Process.unlink(pid)
        pid

      pid ->
        pid
    end
  end

  defp load_config(nil), do: {:ok, nil}
  defp load_config(path), do: Config.load(path)

  ## planning

  defp plan(source, config) do
    tables = Source.tables(source)

    if MapSet.member?(tables, "registered_agents") do
      ledger = Repo.all(from(r in "rust_imports", select: r.key)) |> MapSet.new()

      rows = fn table ->
        if MapSet.member?(tables, table),
          do: Source.all(source, "SELECT * FROM #{table}"),
          else: []
      end

      items =
        Enum.map(rows.("registered_agents"), &plan_agent(&1, ledger)) ++
          Enum.map(rows.("registration_tokens"), &plan_token(&1, ledger)) ++
          Enum.map(rows.("admin_users"), &plan_admin(&1, ledger)) ++
          Enum.map(rows.("api_tokens"), &plan_api_token(&1, ledger)) ++
          plan_config(config, ledger)

      {:ok, items}
    else
      {:error, "that database isn't a Rust coordinator's: it has no registered_agents table"}
    end
  rescue
    e in [Exqlite.Error, Postgrex.Error, ArgumentError] ->
      {:error, "reading the Rust database failed: #{Exception.message(e)}"}
  end

  defp plan_agent(row, ledger) do
    id = row["agent_id"]
    key = ledger_key("agent", id)
    revoked_at = if truthy(row["revoked"]), do: time(row["revoked_at"]) || time(row["created_at"])
    serial = row["serial_number"]

    case Repo.get(Agent, id) do
      nil ->
        if MapSet.member?(ledger, key) do
          item(:agents, id, :skipped, "removed in Phoenix since the last import")
        else
          created = time(row["created_at"])

          agent = %Agent{
            agent_id: id,
            hostname: row["hostname"],
            agent_type: row["agent_type"],
            labels: labels(row["labels"]),
            max_vms: row["max_vms"],
            serial_number: row["serial_number"],
            expires_at: time(row["expires_at"]),
            revoked_at: revoked_at,
            agent_version: row["agent_version"],
            unavailable_at: time(row["unavailable_at"]),
            inserted_at: created,
            updated_at: created
          }

          item(:agents, id, :new, nil, fn -> Repo.insert!(agent) end, key)
        end

      %Agent{serial_number: ^serial} = a ->
        if revoked_at && a.revoked_at == nil do
          item(:agents, id, :revoked, nil, fn ->
            Repo.update_all(from(x in Agent, where: x.agent_id == ^id and is_nil(x.revoked_at)),
              set: [revoked_at: revoked_at, updated_at: DateTime.utc_now()]
            )
          end)
        else
          item(:agents, id, :unchanged)
        end

      _ ->
        item(:agents, id, :conflict, "Phoenix has another certificate for it; kept Phoenix's")
    end
  end

  defp plan_token(row, ledger) do
    token = row["token"]
    label = Agents.token_prefix(token)
    key = ledger_key("registration_token", token)
    expires = time(row["expires_at"])

    cond do
      DateTime.compare(expires, DateTime.utc_now()) != :gt ->
        item(:registration_tokens, label, :skipped, "expired")

      Repo.exists?(from(t in RegistrationToken, where: t.token == ^token), log: false) ->
        item(:registration_tokens, label, :unchanged)

      MapSet.member?(ledger, key) ->
        item(
          :registration_tokens,
          label,
          :skipped,
          "used or deleted in Phoenix since the last import"
        )

      true ->
        rec = %RegistrationToken{
          token: token,
          expires_at: expires,
          created_by: row["created_by"],
          inserted_at: time(row["created_at"])
        }

        item(:registration_tokens, label, :new, nil, fn -> Repo.insert!(rec, log: false) end, key)
    end
  end

  defp plan_admin(row, ledger) do
    name = row["username"]
    key = ledger_key("admin_user", name)

    note =
      if row["totp_secret"] not in [nil, ""],
        do: "its TOTP secret isn't imported (Phoenix has no TOTP)"

    rust_hash = row["password_hash"]

    case Repo.get_by(User, username: name) do
      nil ->
        if MapSet.member?(ledger, key) do
          item(:admin_users, name, :skipped, "deleted in Phoenix since the last import")
        else
          created = time(row["created_at"])

          user = %User{
            username: name,
            password_hash: row["password_hash"],
            last_login_at: time(row["last_login"]),
            inserted_at: created,
            updated_at: created
          }

          item(:admin_users, name, :new, note, fn -> Repo.insert!(user) end, key)
        end

      %User{password_hash: ^rust_hash} ->
        item(:admin_users, name, :unchanged)

      _ ->
        item(:admin_users, name, :conflict, "its password differs in Phoenix; kept Phoenix's")
    end
  end

  defp plan_api_token(row, ledger) do
    label = "#{row["name"]} (#{row["token_prefix"]}…)"

    with {:ok, hash} <- Base.decode16(row["token_hash"] || "", case: :mixed),
         {:ok, id} <- Ecto.UUID.cast(row["id"]) do
      key = ledger_key("api_token", row["token_hash"])

      cond do
        Repo.exists?(from(t in ApiToken, where: t.token_hash == ^hash)) ->
          item(:api_tokens, label, :unchanged)

        MapSet.member?(ledger, key) ->
          item(:api_tokens, label, :skipped, "deleted in Phoenix since the last import")

        Repo.get(ApiToken, id) ->
          item(
            :api_tokens,
            label,
            :conflict,
            "Phoenix has another token with its id; kept Phoenix's"
          )

        true ->
          rec = %ApiToken{
            id: id,
            name: row["name"],
            token_hash: hash,
            token_prefix: row["token_prefix"],
            created_by: row["created_by"],
            last_used_at: time(row["last_used_at"]),
            inserted_at: time(row["created_at"])
          }

          item(:api_tokens, label, :new, nil, fn -> Repo.insert!(rec) end, key)
      end
    else
      _ -> item(:api_tokens, label, :skipped, "its stored hash or id isn't one Phoenix can read")
    end
  end

  defp plan_config(nil, _ledger), do: []

  defp plan_config(config, ledger) do
    Enum.map(config.forges, &plan_forge(&1, ledger)) ++
      Enum.reject(
        [plan_settings(config.settings, ledger), plan_mappings(config.label_mappings, ledger)],
        &is_nil/1
      )
  end

  @compare [
    :type,
    :app_id,
    :private_key,
    :url,
    :token,
    :scopes,
    :webhook_secret,
    :mode,
    :runner_scope
  ]

  defp plan_forge(attrs, ledger) do
    name = attrs.name
    key = ledger_key("forge", name)
    label = "#{name} (#{attrs.type}, #{attrs.mode})"
    cs = Forge.changeset(%Forge{}, attrs)

    case Forges.get_forge_by_name(name) do
      nil ->
        cond do
          MapSet.member?(ledger, key) ->
            item(:forges, label, :skipped, "deleted in Phoenix since the last import")

          not cs.valid? ->
            item(:forges, label, :conflict, "not valid in Phoenix: #{errors(cs)}")

          true ->
            item(
              :forges,
              label,
              :new,
              nil,
              fn ->
                case Forges.create_forge(attrs, validate: false) do
                  {:ok, _} -> :ok
                  {:error, cs} -> Repo.rollback("forge #{name}: #{errors(cs)}")
                end
              end,
              key
            )
        end

      forge ->
        wanted = Ecto.Changeset.apply_changes(cs)
        differs = Enum.reject(@compare, &(Map.get(forge, &1) == Map.get(wanted, &1)))

        if differs == [],
          do: item(:forges, label, :unchanged),
          else:
            item(
              :forges,
              label,
              :conflict,
              "differs in Phoenix (#{Enum.join(differs, ", ")}); kept Phoenix's"
            )
    end
  end

  defp plan_settings(attrs, ledger) do
    key = ledger_key("settings", "1")

    case Repo.get(Settings, 1) do
      nil ->
        if MapSet.member?(ledger, key) do
          item(:settings, "settings", :skipped, "reset in Phoenix since the last import")
        else
          item(
            :settings,
            "settings",
            :new,
            nil,
            fn ->
              case Forges.update_settings(attrs) do
                {:ok, _} -> :ok
                {:error, cs} -> Repo.rollback("provisioning settings: #{errors(cs)}")
              end
            end,
            key
          )
        end

      s ->
        if Enum.all?(attrs, fn {k, v} -> Map.get(s, k) == v end),
          do: item(:settings, "settings", :unchanged),
          else: item(:settings, "settings", :conflict, "Phoenix's differ; kept Phoenix's")
    end
  end

  defp plan_mappings([], _ledger), do: nil

  defp plan_mappings(mappings, ledger) do
    key = ledger_key("label_mappings", "all")
    label = "#{length(mappings)} mapping(s)"
    fields = [:labels, :runner_scope, :runner_group, :forge]
    existing = Forges.list_label_mappings()

    cond do
      Enum.map(existing, &Map.take(&1, fields)) == Enum.map(mappings, &Map.take(&1, fields)) ->
        item(:label_mappings, label, :unchanged)

      existing != [] ->
        item(:label_mappings, label, :conflict, "Phoenix has its own; kept Phoenix's")

      MapSet.member?(ledger, key) ->
        item(:label_mappings, label, :skipped, "deleted in Phoenix since the last import")

      true ->
        item(
          :label_mappings,
          label,
          :new,
          nil,
          fn ->
            for m <- mappings do
              case Forges.create_label_mapping(m) do
                {:ok, _} -> :ok
                {:error, cs} -> Repo.rollback("label mapping #{inspect(m.labels)}: #{errors(cs)}")
              end
            end
          end,
          key
        )
    end
  end

  defp item(kind, label, action, note \\ nil, apply \\ nil, ledger \\ nil),
    do: %{kind: kind, label: label, action: action, note: note, apply: apply, ledger: ledger}

  ## applying

  defp apply_items(items) do
    result =
      Repo.transaction(fn ->
        for %{apply: f} when is_function(f) <- items, do: f.()

        keys = for %{action: a, ledger: k} when a == :new and is_binary(k) <- items, do: %{key: k}

        keys
        |> Enum.chunk_every(1000)
        |> Enum.each(&Repo.insert_all("rust_imports", &1, on_conflict: :nothing))
      end)

    case result do
      {:ok, _} -> :ok
      {:error, msg} -> {:error, "import rolled back, nothing written: #{msg}"}
    end
  end

  ## report

  @doc "The report as text: counts per kind, then each conflict, skip and note."
  @spec format_report(map()) :: iodata()
  def format_report(%{items: items} = r) do
    head = "Rust import from #{r.from}#{if r.config, do: " and #{Path.expand(r.config)}"}"
    head = if r.dry_run, do: head <> " (dry run: nothing written)", else: head

    counts =
      for {kind, name} <- @kinds, its = Enum.filter(items, &(&1.kind == kind)), its != [] do
        by = Enum.frequencies_by(its, & &1.action)

        parts =
          for a <- [:new, :revoked, :unchanged, :skipped, :conflict], n = by[a], do: "#{n} #{a}"

        "#{name}: #{Enum.join(parts, ", ")}"
      end

    details =
      for %{note: n} = i <- items, n != nil or i.action == :conflict do
        "  #{i.action} #{singular(i.kind)} #{i.label}: #{n}"
      end

    forges = for %{kind: :forges, action: :new, label: l} <- items, do: "  new forge #{l}"

    Enum.intersperse([head | counts] ++ forges ++ details, "\n") ++ ["\n"]
  end

  defp singular(:agents), do: "agent"
  defp singular(:registration_tokens), do: "token"
  defp singular(:admin_users), do: "admin user"
  defp singular(:api_tokens), do: "api token"
  defp singular(:forges), do: "forge"
  defp singular(:settings), do: "settings"
  defp singular(:label_mappings), do: "label mappings"

  @doc "True when the report has a conflict (the CLI exits non-zero then)."
  def conflicts?(%{items: items}), do: Enum.any?(items, &(&1.action == :conflict))

  ## helpers

  defp ledger_key(kind, natural),
    do: kind <> ":" <> (:crypto.hash(:sha256, natural) |> Base.encode16(case: :lower))

  defp truthy(v), do: v in [1, true, "1", "true"]

  defp labels(nil), do: []
  defp labels(list) when is_list(list), do: list

  defp labels(text) when is_binary(text) do
    case Jason.decode(text) do
      {:ok, list} when is_list(list) -> list
      _ -> text |> String.split(",", trim: true) |> Enum.map(&String.trim/1)
    end
  end

  # Rust stores chrono's to_rfc3339 text (nanoseconds, an offset)
  defp time(nil), do: nil
  defp time(""), do: nil

  defp time(text) when is_binary(text) do
    case DateTime.from_iso8601(text) do
      {:ok, dt, _} ->
        DateTime.truncate(dt, :microsecond) |> to_usec()

      _ ->
        raise ArgumentError, "the Rust database has a time Phoenix can't read: #{inspect(text)}"
    end
  end

  defp to_usec(%DateTime{microsecond: {us, _}} = dt), do: %{dt | microsecond: {us, 6}}

  defp errors(%Ecto.Changeset{} = cs) do
    cs
    |> Ecto.Changeset.traverse_errors(fn {msg, opts} ->
      Enum.reduce(opts, msg, fn {k, v}, acc -> String.replace(acc, "%{#{k}}", to_string(v)) end)
    end)
    |> Enum.map_join("; ", fn {f, msgs} -> "#{f} #{Enum.join(List.wrap(msgs), ", ")}" end)
  end
end
