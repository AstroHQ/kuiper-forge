defmodule KuiperCoordinator.Repo.Migrations.CreateRunHistory do
  use Ecto.Migration

  # Rust run history (#67, migrations/shared/20261029000000 + 20261031000000): one row per runner
  # attempt and one per job, for the metrics page. Times are unix millis so durations are a subtraction.
  def change do
    create table(:runner_runs, primary_key: false) do
      add :runner_name, :string, primary_key: true
      add :agent_id, :string, null: false
      # copied in: the agent row goes away after it's revoked
      add :hostname, :string, null: false
      add :agent_type, :string, null: false
      add :forge, :string
      add :job_key, :string
      add :repository, :string
      add :workflow_name, :string
      add :job_name, :string
      add :pool, :string
      add :labels, {:array, :string}, null: false, default: []
      # CreateRunner sent
      add :requested_at, :bigint, null: false
      add :acked_at, :bigint
      # agent's STARTED event: runner set up, about to take the job
      add :started_at, :bigint
      # the forge says the job is running
      add :job_started_at, :bigint
      add :finished_at, :bigint
      # completed, failed, lost, rejected, command_failed, idled, drained, destroyed
      add :outcome, :string
      add :error, :text
      add :image, :string
      add :failed_phase, :string
      # hit, filled, vm or fell_back; nil when the image had it, or for older agents
      add :install_source, :string
      # step name -> millis as the agent timed them; open ended so agents can add steps
      add :steps, :map, null: false, default: %{}
    end

    create index(:runner_runs, [:requested_at])
    create index(:runner_runs, [:agent_id, :requested_at])

    create table(:job_runs, primary_key: false) do
      # "forge:id", like pending_jobs
      add :job_key, :string, primary_key: true
      add :forge, :string, null: false
      add :repository, :string
      add :workflow_name, :string
      add :job_name, :string
      add :labels, {:array, :string}, null: false, default: []
      # the forge's own created time when it gives one (never after seen_at), else seen_at
      add :queued_at, :bigint, null: false
      # first added to the pending queue
      add :seen_at, :bigint, null: false
      add :first_runner_at, :bigint
      add :started_at, :bigint
      add :completed_at, :bigint
      # the forge's verdict (success, failure, cancelled, ...)
      add :conclusion, :string
      # whichever runner the forge says took it
      add :runner_name, :string
      add :attempts, :integer, null: false, default: 0
      # ran, abandoned, gone, cancelled; nil while pending
      add :outcome, :string
    end

    create index(:job_runs, [:seen_at])
  end
end
