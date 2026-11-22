defmodule KuiperCoordinator.Repo.Migrations.CreateApiTokens do
  use Ecto.Migration

  def change do
    # the token itself is never stored: only its SHA-256 and a short prefix for display
    create table(:api_tokens, primary_key: false) do
      add :id, :binary_id, primary_key: true
      add :name, :string, null: false
      add :token_hash, :binary, null: false
      add :token_prefix, :string, null: false
      add :created_by, :string, null: false
      add :last_used_at, :utc_datetime_usec

      timestamps(type: :utc_datetime_usec, updated_at: false)
    end

    create unique_index(:api_tokens, [:token_hash])
  end
end
