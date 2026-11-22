defmodule KuiperCoordinator.Forges.LabelMapping do
  @moduledoc """
  Routes jobs with these labels to a runner scope and group, optionally on one forge (Rust
  `[[webhook.label_mappings]]`; the first match in `position` order wins).
  """
  use Ecto.Schema
  import Ecto.Changeset

  alias KuiperCoordinator.Forge.RunnerScope

  schema "label_mappings" do
    field :labels, {:array, :string}
    field :runner_scope, :string
    field :runner_group, :string
    field :forge, :string
    field :position, :integer, default: 0
    timestamps(type: :utc_datetime_usec)
  end

  def changeset(%__MODULE__{} = m, attrs) do
    m
    |> cast(attrs, [:labels, :runner_scope, :runner_group, :forge, :position],
      empty_values: [nil, ""]
    )
    |> update_change(:labels, fn l ->
      l |> Enum.map(&String.trim/1) |> Enum.reject(&(&1 == ""))
    end)
    |> validate_required([:labels])
    |> validate_length(:labels, min: 1)
    |> validate_change(:runner_scope, fn :runner_scope, scope ->
      case RunnerScope.parse(scope) do
        {:ok, _} -> []
        {:error, msg} -> [runner_scope: msg]
      end
    end)
  end
end
