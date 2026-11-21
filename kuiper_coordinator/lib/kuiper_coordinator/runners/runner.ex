defmodule KuiperCoordinator.Runners.Runner do
  @moduledoc """
  A runner the coordinator created on an agent. `pool` is the explicit pool key
  (`labels@set_id`, parity R6) or nil; `job_key` is set for job-bound runners.
  """
  use Ecto.Schema

  @primary_key {:runner_name, :string, autogenerate: false}
  schema "runners" do
    field :agent_id, :string
    field :vm_name, :string
    field :runner_scope, KuiperCoordinator.Forge.RunnerScope.Type
    field :forge, :string
    field :job_key, :string
    field :job_name, :string
    field :repository, :string
    field :workflow_name, :string
    field :pool, :string
    field :labels, {:array, :string}, default: []

    timestamps(type: :utc_datetime_usec)
  end
end
