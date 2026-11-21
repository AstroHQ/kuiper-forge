defmodule KuiperCoordinator.Repo.Migrations.CreatePendingJobs do
  use Ecto.Migration

  # Queued forge jobs waiting for a runner (Rust: pending_webhook_jobs). The row is written
  # before the webhook answers, so a full fleet never loses a job (parity J1).
  def change do
    create table(:pending_jobs, primary_key: false) do
      add :job_key, :string, primary_key: true
      add :forge, :string, null: false
      add :job_labels, {:array, :string}, null: false, default: []
      add :agent_labels, {:array, :string}, null: false, default: []
      add :runner_scope, :string, null: false
      add :runner_group, :string
      add :repository, :string
      add :job_name, :string
      add :workflow_name, :string
      add :queued_at, :utc_datetime_usec
      add :retry_count, :integer, null: false, default: 0
      add :failed_agents, {:array, :string}, null: false, default: []

      timestamps(type: :utc_datetime_usec)
    end

    create index(:pending_jobs, [:inserted_at])
  end
end
