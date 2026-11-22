defmodule KuiperCoordinator.Admin.FirstUserRaceTest do
  @moduledoc """
  D50 + Codex P1 on #93: the CLI (`Release.create_admin/1` -> `Admin.create_user/3`) and the
  setup page (`Admin.create_first_user/3`) share one advisory lock, so a setup that runs while a
  CLI create is still uncommitted waits for it, sees the user, and backs off. Runs outside the
  SQL sandbox so the two are real concurrent transactions.
  """
  use ExUnit.Case, async: false

  alias KuiperCoordinator.{Admin, Repo}
  alias KuiperCoordinator.Admin.User

  setup do
    :ok = Ecto.Adapters.SQL.Sandbox.checkout(Repo, sandbox: false)

    on_exit(fn ->
      :ok = Ecto.Adapters.SQL.Sandbox.checkout(Repo, sandbox: false)
      Repo.delete_all(User)
    end)

    Repo.delete_all(User)
    :ok
  end

  test "setup during an uncommitted CLI create waits, then refuses: one admin" do
    parent = self()

    cli =
      Task.async(fn ->
        :ok = Ecto.Adapters.SQL.Sandbox.checkout(Repo, sandbox: false)

        Repo.transaction(fn ->
          {:ok, _} = Admin.create_user("cli-admin", "pw pw pw", "pw pw pw")
          send(parent, :cli_inserted)
          # hold the CLI's insert uncommitted while setup runs
          receive do
            :commit -> :ok
          after
            10_000 -> :ok
          end
        end)
      end)

    assert_receive :cli_inserted, 10_000

    setup =
      Task.async(fn ->
        :ok = Ecto.Adapters.SQL.Sandbox.checkout(Repo, sandbox: false)
        Admin.create_first_user("web-admin", "pw pw pw", "pw pw pw")
      end)

    # setup must still be waiting on the lock while the CLI create is open
    assert Task.yield(setup, 300) == nil
    send(cli.pid, :commit)
    assert {:ok, _} = Task.await(cli, 10_000)

    assert Task.await(setup, 10_000) == {:error, :setup_done}
    assert [%{username: "cli-admin"}] = Admin.list_users()
  end

  test "racing CLI creates and setups on an empty DB: setup wins only as the first user" do
    for round <- 1..10 do
      Repo.delete_all(User)

      tasks =
        for i <- 1..4 do
          Task.async(fn ->
            :ok = Ecto.Adapters.SQL.Sandbox.checkout(Repo, sandbox: false)

            if rem(i, 2) == 0 do
              {:cli, Admin.create_user("cli#{round}-#{i}", "pw pw pw", "pw pw pw")}
            else
              {:setup, Admin.create_first_user("web#{round}-#{i}", "pw pw pw", "pw pw pw")}
            end
          end)
        end

      results = Enum.map(tasks, &Task.await(&1, 30_000))
      setups_ok = for {:setup, {:ok, u}} <- results, do: u
      clis_ok = for {:cli, {:ok, u}} <- results, do: u

      # the CLI always works (reset/recovery tool); at most one setup wins, and only if it was
      # the very first user, so no CLI user was committed before it
      assert length(clis_ok) == 2
      assert length(setups_ok) <= 1

      for s <- setups_ok, c <- clis_ok do
        assert s.id < c.id, "setup made #{s.username} after CLI made #{c.username}"
      end

      assert Admin.user_count() == 2 + length(setups_ok)
    end
  end
end
