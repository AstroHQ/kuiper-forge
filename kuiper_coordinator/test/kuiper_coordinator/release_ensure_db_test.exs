defmodule KuiperCoordinator.ReleaseEnsureDbTest do
  # bin/migrate makes a missing database, e.g. the <db>_phoenix one named from the Rust
  # coordinator's KUIPER_DATABASE__* (D75), instead of failing on it
  use ExUnit.Case, async: true
  import ExUnit.CaptureIO

  alias Ecto.Adapters.Postgres
  alias KuiperCoordinator.Release

  defp config(database) do
    KuiperCoordinator.Repo.config()
    |> Keyword.drop([:pool, :url])
    |> Keyword.put(:database, database)
  end

  test "creates a missing database, then leaves it be" do
    config = config("kc_ensure_db_#{System.unique_integer([:positive])}")
    on_exit(fn -> Postgres.storage_down(config) end)

    assert capture_io(:stderr, fn -> assert Release.ensure_db(Postgres, config) == :created end) =~
             "created the #{config[:database]} database"

    assert Postgres.storage_status(config) == :up
    assert Release.ensure_db(Postgres, config) == :ok
  end

  test "a role that can't create it gets a warning, not a crash" do
    config = config("kc_ensure_db_nope") |> Keyword.put(:username, "kc_no_such_role")

    assert capture_io(:stderr, fn ->
             assert {:error, _} = Release.ensure_db(Postgres, config)
           end) =~ "could not create the kc_ensure_db_nope database"
  end
end
