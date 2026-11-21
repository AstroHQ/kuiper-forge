defmodule KuiperCoordinator.Fleet.PoolDefinition do
  @moduledoc """
  A fixed-capacity pool derived from connected agents (Rust agent_registry.rs
  get_pool_definitions, parity R6). An agent that sets `pool` on its mappings gets one explicit
  pool per label set (`agent_id` set). Other available agents share a legacy pool per normalized
  base-label set; `members` are those agents, whose runners all count toward it (P3).
  """

  defstruct labels: [], target_count: 0, agent_id: nil, label_set_id: "", members: []

  @type t :: %__MODULE__{
          labels: [String.t()],
          target_count: non_neg_integer(),
          agent_id: String.t() | nil,
          label_set_id: String.t(),
          members: [String.t()]
        }

  @doc "Lowercased and sorted, so the same labels in any order make the same pool."
  @spec normalize_labels([String.t()]) :: [String.t()]
  def normalize_labels(labels), do: labels |> Enum.map(&String.downcase/1) |> Enum.sort()

  @doc "Key stored on an explicit pool's runners; two sets can share labels, so it has the id."
  @spec key(t()) :: String.t()
  def key(%__MODULE__{labels: labels, label_set_id: ""}), do: Enum.join(labels, ",")
  def key(%__MODULE__{labels: labels, label_set_id: id}), do: Enum.join(labels, ",") <> "@" <> id

  def explicit?(%__MODULE__{agent_id: id}), do: not is_nil(id)
end
