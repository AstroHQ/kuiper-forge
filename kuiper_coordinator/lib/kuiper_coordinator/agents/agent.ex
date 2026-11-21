defmodule KuiperCoordinator.Agents.Agent do
  @moduledoc "A registered agent: one row per issued client certificate (auth.rs RegisteredAgent)."
  use Ecto.Schema

  @primary_key {:agent_id, :string, autogenerate: false}
  @timestamps_opts [type: :utc_datetime_usec]
  schema "agents" do
    field :hostname, :string
    field :agent_type, :string
    field :labels, {:array, :string}, default: []
    field :max_vms, :integer, default: 0
    field :serial_number, :string
    field :expires_at, :utc_datetime_usec
    field :revoked_at, :utc_datetime_usec
    field :agent_version, :string
    field :unavailable_at, :utc_datetime_usec

    timestamps()
  end
end
