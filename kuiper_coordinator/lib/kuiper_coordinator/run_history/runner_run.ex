defmodule KuiperCoordinator.RunHistory.RunnerRun do
  @moduledoc "One runner attempt in the run history (Rust metrics RunnerRun). Times are unix millis."
  use Ecto.Schema

  @primary_key {:runner_name, :string, autogenerate: false}
  schema "runner_runs" do
    field :agent_id, :string
    field :hostname, :string
    field :agent_type, :string
    field :forge, :string
    field :job_key, :string
    field :repository, :string
    field :workflow_name, :string
    field :job_name, :string
    field :pool, :string
    field :labels, {:array, :string}, default: []
    field :requested_at, :integer
    field :acked_at, :integer
    field :started_at, :integer
    field :job_started_at, :integer
    field :finished_at, :integer
    field :outcome, :string
    field :error, :string
    field :image, :string
    field :failed_phase, :string
    field :install_source, :string
    field :steps, :map, default: %{}
  end
end
