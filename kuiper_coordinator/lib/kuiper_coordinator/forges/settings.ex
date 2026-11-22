defmodule KuiperCoordinator.Forges.Settings do
  @moduledoc """
  How runners are provisioned (one row, id 1; D39): the mode (`fixed_capacity`, `webhook`,
  `polling`), the fixed-capacity runner scope and group, the idle teardown timeout (default
  600 s, at least 120, 0 off), the poll interval (default 120 s, at least 10), the forge scan's
  lookback, the labels a job must carry, and the webhook's path and source forge.
  """
  use Ecto.Schema
  import Ecto.Changeset

  alias KuiperCoordinator.Forge.RunnerScope

  @min_idle_s 120
  @min_poll_s 10

  schema "provisioning_settings" do
    field :mode, Ecto.Enum,
      values: [:fixed_capacity, :webhook, :polling],
      default: :fixed_capacity

    field :runner_scope, :string
    field :runner_group, :string
    field :idle_runner_timeout_s, :integer, default: 600
    field :poll_interval_s, :integer, default: 120
    field :scan_lookback_hours, :integer, default: 168
    field :required_labels, {:array, :string}, default: ["self-hosted"]
    field :webhook_path, :string, default: "/webhook"
    field :webhook_forge, :string
    timestamps(type: :utc_datetime_usec)
  end

  def changeset(%__MODULE__{} = s, attrs) do
    s
    |> cast(
      attrs,
      [
        :mode,
        :runner_scope,
        :runner_group,
        :idle_runner_timeout_s,
        :poll_interval_s,
        :scan_lookback_hours,
        :required_labels,
        :webhook_path,
        :webhook_forge
      ],
      empty_values: [nil, ""]
    )
    |> validate_required([:mode, :idle_runner_timeout_s, :poll_interval_s, :scan_lookback_hours])
    |> validate_number(:idle_runner_timeout_s, greater_than_or_equal_to: 0)
    |> validate_number(:poll_interval_s, greater_than_or_equal_to: @min_poll_s)
    |> validate_number(:scan_lookback_hours, greater_than: 0)
    |> update_change(:idle_runner_timeout_s, &if(&1 > 0, do: max(&1, @min_idle_s), else: &1))
    |> update_change(:required_labels, fn l ->
      l |> Enum.map(&String.trim/1) |> Enum.reject(&(&1 == ""))
    end)
    |> validate_format(:webhook_path, ~r{^/}, message: "must start with /")
    |> validate_change(:runner_scope, fn :runner_scope, scope ->
      case RunnerScope.parse(scope) do
        {:ok, _} -> []
        {:error, msg} -> [runner_scope: msg]
      end
    end)
  end
end
