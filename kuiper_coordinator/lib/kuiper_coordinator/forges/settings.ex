defmodule KuiperCoordinator.Forges.Settings do
  @moduledoc """
  What every forge's provisioning shares (one row, id 1; D39): the idle teardown timeout
  (default 600 s, at least 120, 0 off), the forge scan's lookback, the labels a job must carry,
  and the path webhooks are delivered to. The mode is each forge's own
  (`KuiperCoordinator.Forges.Forge`).
  """
  use Ecto.Schema
  import Ecto.Changeset

  @min_idle_s 120

  schema "provisioning_settings" do
    field :idle_runner_timeout_s, :integer, default: 600
    field :scan_lookback_hours, :integer, default: 168
    field :required_labels, {:array, :string}, default: ["self-hosted"]
    field :webhook_path, :string, default: "/webhook"
    timestamps(type: :utc_datetime_usec)
  end

  def changeset(%__MODULE__{} = s, attrs) do
    s
    |> cast(
      attrs,
      [:idle_runner_timeout_s, :scan_lookback_hours, :required_labels, :webhook_path],
      empty_values: [nil, ""]
    )
    |> validate_required([:idle_runner_timeout_s, :scan_lookback_hours])
    |> validate_number(:idle_runner_timeout_s, greater_than_or_equal_to: 0)
    |> validate_number(:scan_lookback_hours, greater_than_or_equal_to: 0)
    |> update_change(:idle_runner_timeout_s, &if(&1 > 0, do: max(&1, @min_idle_s), else: &1))
    |> update_change(:required_labels, fn l ->
      l |> Enum.map(&String.trim/1) |> Enum.reject(&(&1 == ""))
    end)
    |> validate_format(:webhook_path, ~r{^/}, message: "must start with /")
  end
end
