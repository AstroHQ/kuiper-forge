defmodule KuiperCoordinator.RunHistory.JobRun do
  @moduledoc "One job in the run history (Rust metrics JobRun). Times are unix millis."
  use Ecto.Schema

  @primary_key {:job_key, :string, autogenerate: false}
  schema "job_runs" do
    field :forge, :string
    field :repository, :string
    field :workflow_name, :string
    field :job_name, :string
    field :labels, {:array, :string}, default: []
    field :queued_at, :integer
    field :seen_at, :integer
    field :first_runner_at, :integer
    field :started_at, :integer
    field :completed_at, :integer
    field :conclusion, :string
    field :runner_name, :string
    field :attempts, :integer, default: 0
    field :outcome, :string
  end
end
