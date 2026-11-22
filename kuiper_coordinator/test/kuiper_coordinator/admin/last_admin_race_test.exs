defmodule KuiperCoordinator.Admin.LastAdminRaceTest do
  @moduledoc """
  Parity UI8 (#32, 176390d): two admins deleting each other at once must not both succeed. Runs
  outside the SQL sandbox so the two deletes are real concurrent transactions.
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

  test "two concurrent cross deletes leave one admin" do
    for _ <- 1..10 do
      Repo.delete_all(User)
      {:ok, _} = Admin.create_user("alice", "pw", "pw")
      {:ok, _} = Admin.create_user("bob", "pw", "pw")

      results =
        [{"bob", "alice"}, {"alice", "bob"}]
        |> Enum.map(fn {target, actor} ->
          Task.async(fn ->
            :ok = Ecto.Adapters.SQL.Sandbox.checkout(Repo, sandbox: false)
            Admin.delete_user(target, actor)
          end)
        end)
        |> Enum.map(&Task.await/1)

      assert Enum.sort(results) == [:ok, {:error, :last_admin}]
      assert Repo.aggregate(User, :count) == 1
    end
  end
end
