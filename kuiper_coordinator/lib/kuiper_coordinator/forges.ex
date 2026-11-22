defmodule KuiperCoordinator.Forges do
  @moduledoc """
  Forges and provisioning settings, stored in Postgres and edited from the admin UI (D39, D41).
  Env vars only bootstrap the app (database, CA, ports, `KUIPER_ENCRYPTION_KEY`).

  Every change is broadcast as `{:forges_changed}` on PubSub topic `topic/0`;
  `KuiperCoordinator.Fleet.Loader` rebuilds the fleet from `fleet_config/0` when it hears one,
  without a restart. A forge that doesn't check out is rejected when it's saved: its fields
  (`Forges.Forge.changeset/2`) and, unless `validate: false`, a live check against the forge's
  API (the GitHub App has installations, the Forgejo token reads each scope).
  """
  import Ecto.Query

  alias KuiperCoordinator.Repo
  alias KuiperCoordinator.Forge.RunnerScope
  alias KuiperCoordinator.Forges.{Forge, LabelMapping, Settings}

  @topic "forges"
  @settings_id 1

  def topic, do: @topic

  @doc "Subscribes the caller to `{:forges_changed}`."
  def subscribe, do: Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, @topic)

  ## forges

  @doc "Forges, GitHub first, then in the order they were added."
  def list_forges do
    Forge
    |> order_by([f], asc: f.inserted_at, asc: f.id)
    |> Repo.all()
    |> Enum.sort_by(&(&1.type != :github))
  end

  def get_forge!(id), do: Repo.get!(Forge, id)
  def get_forge_by_name(name), do: Repo.get_by(Forge, name: name)

  @doc "A changeset for a form (secrets aren't filled back in)."
  def change_forge(%Forge{} = forge, attrs \\ %{}), do: Forge.changeset(forge, attrs)

  @doc """
  Saves a new forge after a live check. Options: `:validate` (default
  `config :kuiper_coordinator, :validate_forges_on_save`, true), `:req_options` (tests).
  """
  def create_forge(attrs, opts \\ []), do: save(Forge.changeset(%Forge{}, attrs), opts)

  @doc "Updates a forge (blank secrets keep the stored ones), with the same check."
  def update_forge(%Forge{} = forge, attrs, opts \\ []),
    do: save(Forge.changeset(forge, attrs), opts)

  def delete_forge(%Forge{} = forge), do: forge |> Repo.delete() |> notify()

  defp save(cs, opts) do
    with {:ok, cs} <- cs |> one_fixed_capacity() |> live_check(opts) do
      cs |> Repo.insert_or_update() |> notify()
    end
  end

  # the agents' pools name no forge, so only one forge can fill them
  defp one_fixed_capacity(cs) do
    with :fixed_capacity <- Ecto.Changeset.get_field(cs, :mode),
         %Forge{name: other} <- fixed_capacity_forge(cs.data.id) do
      Ecto.Changeset.add_error(
        cs,
        :mode,
        "forge '#{other}' already uses fixed capacity, and the pools fill one forge"
      )
    else
      _ -> cs
    end
  end

  defp fixed_capacity_forge(except_id) do
    Forge
    |> where([f], f.mode == :fixed_capacity)
    |> then(&if(except_id, do: where(&1, [f], f.id != ^except_id), else: &1))
    |> first(:id)
    |> Repo.one()
  end

  defp live_check(%Ecto.Changeset{valid?: false} = cs, _opts),
    do: {:error, %{cs | action: action(cs)}}

  defp live_check(cs, opts) do
    validate? =
      Keyword.get_lazy(opts, :validate, fn ->
        Application.get_env(:kuiper_coordinator, :validate_forges_on_save, true)
      end)

    if validate? do
      spec = cs |> Ecto.Changeset.apply_changes() |> spec(get_settings().scan_lookback_hours)
      req_options = Keyword.get(opts, :req_options, [])

      # in a task: the clients' caches are linked to the process that builds them
      task =
        Task.async(fn ->
          KuiperCoordinator.Fleet.Forges.build([spec], validate: true, req_options: req_options)
        end)

      case Task.yield(task, 30_000) || Task.shutdown(task) do
        {:ok, {:ok, _}} ->
          {:ok, cs}

        {:ok, {:error, msg}} ->
          {:error, %{Ecto.Changeset.add_error(cs, :base, msg) | action: action(cs)}}

        _ ->
          {:error,
           %{
             Ecto.Changeset.add_error(cs, :base, "the forge didn't answer in 30 s")
             | action: action(cs)
           }}
      end
    else
      {:ok, cs}
    end
  end

  defp action(%Ecto.Changeset{data: %{id: nil}}), do: :insert
  defp action(_), do: :update

  @doc false
  def spec(%Forge{} = f, lookback), do: Map.merge(client_spec(f, lookback), provisioning(f))

  defp client_spec(%Forge{type: :github} = f, lookback),
    do: %{
      name: f.name,
      type: :github,
      app_id: f.app_id,
      private_key: f.private_key,
      scan_lookback_hours: lookback
    }

  defp client_spec(%Forge{type: :forgejo} = f, _lookback),
    do: %{name: f.name, type: :forgejo, url: f.url, token: f.token, scopes: f.scopes}

  defp provisioning(%Forge{} = f),
    do: %{
      mode: f.mode,
      runner_scope: f.runner_scope && elem(RunnerScope.parse(f.runner_scope), 1),
      poll_interval_s: f.poll_interval_s
    }

  ## settings

  @doc "The provisioning settings (defaults until they're first saved)."
  def get_settings, do: Repo.get(Settings, @settings_id) || %Settings{id: @settings_id}

  def change_settings(%Settings{} = s, attrs \\ %{}), do: Settings.changeset(s, attrs)

  def update_settings(attrs) do
    get_settings() |> Settings.changeset(attrs) |> Repo.insert_or_update() |> notify()
  end

  ## label mappings

  def list_label_mappings do
    LabelMapping |> order_by([m], asc: m.position, asc: m.id) |> Repo.all()
  end

  def get_label_mapping!(id), do: Repo.get!(LabelMapping, id)

  def change_label_mapping(%LabelMapping{} = m, attrs \\ %{}),
    do: LabelMapping.changeset(m, attrs)

  def create_label_mapping(attrs),
    do: %LabelMapping{} |> LabelMapping.changeset(attrs) |> Repo.insert() |> notify()

  def update_label_mapping(%LabelMapping{} = m, attrs),
    do: m |> LabelMapping.changeset(attrs) |> Repo.update() |> notify()

  def delete_label_mapping(%LabelMapping{} = m), do: m |> Repo.delete() |> notify()

  defp notify({:ok, _} = ok) do
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, @topic, {:forges_changed})
    ok
  end

  defp notify(err), do: err

  ## the fleet

  @doc """
  What the fleet runs on, from the stored forges and settings: `{:ok, %{fleet: opts | nil,
  webhook: opts}}` (`fleet: nil` with no forges: nothing is provisioned) or `{:error, reason}`
  when the forges don't fit together. The admin UI can show the reason; the fleet stays off
  until it's fixed.

  Each forge spec carries its own mode. `webhook` is what the webhook plugs read: the path, the
  job filters, and a source (`%{forge: name, secret: secret}`) for every forge in webhook mode.
  """
  @spec fleet_config() ::
          {:ok, %{fleet: keyword() | nil, webhook: keyword()}} | {:error, String.t()}
  def fleet_config do
    case list_forges() do
      [] -> {:ok, %{fleet: nil, webhook: []}}
      forges -> fleet_config(forges, get_settings(), list_label_mappings())
    end
  end

  defp fleet_config(forges, s, mappings) do
    # saving a forge refuses a second one; this is for rows that got there another way
    case for(%Forge{mode: :fixed_capacity, name: name} <- forges, do: name) do
      [_, _ | _] = names ->
        {:error,
         "only one forge can use fixed capacity (#{Enum.join(names, ", ")} do): " <>
           "the agents' pools fill one forge"}

      _ ->
        fleet = [
          forges: Enum.map(forges, &spec(&1, s.scan_lookback_hours)),
          idle_runner_timeout_s: s.idle_runner_timeout_s
        ]

        sources =
          for %Forge{mode: :webhook} = f <- forges, do: %{forge: f.name, secret: f.webhook_secret}

        {:ok,
         %{
           fleet: fleet,
           webhook: [path: s.webhook_path, sources: sources] ++ filters(s, mappings)
         }}
    end
  end

  defp filters(s, mappings) do
    [
      required_labels: s.required_labels,
      label_mappings:
        Enum.map(mappings, fn m ->
          %{
            labels: m.labels,
            runner_scope: m.runner_scope && elem(RunnerScope.parse(m.runner_scope), 1),
            runner_group: m.runner_group,
            forge: m.forge
          }
        end)
    ]
  end
end
