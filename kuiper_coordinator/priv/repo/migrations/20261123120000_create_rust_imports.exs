defmodule KuiperCoordinator.Repo.Migrations.CreateRustImports do
  use Ecto.Migration

  # what `mix kuiper.import_rust` brought over, so a rerun never brings back what Phoenix has
  # since removed (a used registration token, a deleted user). keys are hashed: never a token
  def change do
    create table(:rust_imports, primary_key: false) do
      add :key, :string, primary_key: true
      add :inserted_at, :utc_datetime_usec, null: false, default: fragment("now()")
    end
  end
end
