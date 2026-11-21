defmodule KuiperCoordinator.Repo.Migrations.CreateAgentFailures do
  use Ecto.Migration

  # Rust agent_failures: newest 100 per agent, for the agent page and the errors page (#33, #74)
  def change do
    create table(:agent_failures, primary_key: false) do
      add :id, :binary_id, primary_key: true
      add :agent_id, :string, null: false
      add :occurred_at, :utc_datetime_usec, null: false
      # kept as a string so old rows survive new kinds
      add :kind, :string, null: false
      add :runner_name, :string
      add :job_key, :string
      add :message, :text, null: false
    end

    create index(:agent_failures, [:agent_id, :occurred_at])
    create index(:agent_failures, [:occurred_at])
  end
end
