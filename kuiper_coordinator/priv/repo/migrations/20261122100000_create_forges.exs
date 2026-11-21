defmodule KuiperCoordinator.Repo.Migrations.CreateForges do
  use Ecto.Migration

  def change do
    create table(:forges) do
      add :name, :string, null: false
      add :type, :string, null: false
      # GitHub App
      add :app_id, :bigint
      add :private_key, :binary
      # Forgejo
      add :url, :string
      add :token, :binary
      add :scopes, {:array, :string}, null: false, default: []
      # both; used when this forge is the webhook's source
      add :webhook_secret, :binary
      timestamps(type: :utc_datetime_usec)
    end

    create unique_index(:forges, [:name])

    # one row (id 1): how runners are provisioned
    create table(:provisioning_settings) do
      add :mode, :string, null: false, default: "fixed_capacity"
      add :runner_scope, :string
      add :runner_group, :string
      add :idle_runner_timeout_s, :integer, null: false, default: 600
      add :poll_interval_s, :integer, null: false, default: 120
      add :scan_lookback_hours, :integer, null: false, default: 168
      add :required_labels, {:array, :string}, null: false, default: ["self-hosted"]
      add :webhook_path, :string, null: false, default: "/webhook"
      add :webhook_forge, :string
      timestamps(type: :utc_datetime_usec)
    end

    create table(:label_mappings) do
      add :labels, {:array, :string}, null: false
      add :runner_scope, :string
      add :runner_group, :string
      add :forge, :string
      add :position, :integer, null: false, default: 0
      timestamps(type: :utc_datetime_usec)
    end
  end
end
