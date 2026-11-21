defmodule KuiperCoordinator.Agents.LogLine do
  @moduledoc "One uploaded agent log line (agent_logs.rs). `ts` is Unix micros, `level` ERROR=1..TRACE=5."
  use Ecto.Schema

  schema "agent_logs" do
    field :agent_id, :string
    field :ts, :integer
    field :seq, :integer, default: 0
    field :level, :integer
    field :target, :string, default: ""
    field :message, :string
    # set by the database on insert; retention goes by this, not the agent's ts
    field :inserted_at, :utc_datetime_usec, read_after_writes: true
  end
end
