defmodule KuiperCoordinator.Jobs.PendingJob do
  @moduledoc "A queued forge job waiting for a runner. `job_key` is `forge:id` (parity T2)."
  use Ecto.Schema

  @primary_key {:job_key, :string, autogenerate: false}
  schema "pending_jobs" do
    field :forge, :string
    field :job_labels, {:array, :string}, default: []
    field :agent_labels, {:array, :string}, default: []
    field :runner_scope, KuiperCoordinator.Forge.RunnerScope.Type
    field :runner_group, :string
    field :repository, :string
    field :job_name, :string
    field :workflow_name, :string
    field :queued_at, :utc_datetime_usec
    field :retry_count, :integer, default: 0
    field :failed_agents, {:array, :string}, default: []

    timestamps(type: :utc_datetime_usec)
  end
end
