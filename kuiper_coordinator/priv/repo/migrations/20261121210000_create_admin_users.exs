defmodule KuiperCoordinator.Repo.Migrations.CreateAdminUsers do
  use Ecto.Migration

  def change do
    create table(:admin_users) do
      add :username, :string, null: false
      add :password_hash, :string, null: false
      add :last_login_at, :utc_datetime_usec

      timestamps(type: :utc_datetime_usec)
    end

    create unique_index(:admin_users, [:username])

    # session id is 64 alphanumerics in the cookie; only its SHA-256 is stored
    create table(:admin_sessions) do
      add :user_id, references(:admin_users, on_delete: :delete_all), null: false
      add :token_hash, :binary, null: false
      add :expires_at, :utc_datetime_usec, null: false
      add :ip_address, :string
      add :user_agent, :text

      timestamps(type: :utc_datetime_usec, updated_at: false)
    end

    create unique_index(:admin_sessions, [:token_hash])
    create index(:admin_sessions, [:user_id])
  end
end
