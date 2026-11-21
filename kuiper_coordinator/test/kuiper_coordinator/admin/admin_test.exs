defmodule KuiperCoordinator.AdminTest do
  @moduledoc """
  Admin users and sessions (parity UI1, UI8, UI10): Argon2id PHC hashes, 64-char session ids
  with a fixed expiry that is never extended, the name rule, set-password killing that user's
  other sessions, and the last-admin check holding under concurrent deletes.
  """
  use KuiperCoordinator.DataCase, async: false

  alias KuiperCoordinator.Admin
  alias KuiperCoordinator.Admin.{Session, User}

  defp user!(name, pw \\ "correct horse") do
    {:ok, u} = Admin.create_user(name, pw, pw)
    u
  end

  describe "create_user/3" do
    test "stores an Argon2id PHC hash, never the password" do
      u = user!("alice")
      assert "$argon2id$" <> _ = u.password_hash
      refute u.password_hash =~ "correct horse"
    end

    test "name is 1-64 of [A-Za-z0-9._-@], trimmed; password non-empty and confirmed" do
      assert {:ok, %User{username: "ops.bot-1_x@example"}} =
               Admin.create_user("  ops.bot-1_x@example ", "pw", "pw")

      for bad <- ["", "   ", String.duplicate("a", 65), "has space", "<script>", "a/b"] do
        assert {:error, :invalid_username} = Admin.create_user(bad, "pw", "pw"), inspect(bad)
      end

      assert {:ok, _} = Admin.create_user(String.duplicate("a", 64), "pw", "pw")
      assert {:error, :empty_password} = Admin.create_user("bob", "", "")
      assert {:error, :password_mismatch} = Admin.create_user("bob", "pw", "pW")
    end

    test "names are unique" do
      user!("alice")
      assert {:error, :username_taken} = Admin.create_user("alice", "pw", "pw")
    end
  end

  describe "sessions" do
    test "login gives a 64-char session id; only its hash is stored" do
      user!("alice")
      assert {:ok, token, %User{username: "alice"}} = Admin.login("alice", "correct horse", %{})
      assert token =~ ~r/\A[A-Za-z0-9]{64}\z/
      refute Repo.get_by(Session, token_hash: token)
      assert %User{username: "alice"} = Admin.session_user(token)
      assert Repo.get_by!(User, username: "alice").last_login_at
    end

    test "wrong password and unknown user both fail the same way" do
      user!("alice")
      assert :error = Admin.login("alice", "nope", %{})
      assert :error = Admin.login("mallory", "correct horse", %{})
      assert Repo.aggregate(Session, :count) == 0
    end

    test "an unknown user still pays for a hash (no timing oracle, UI10)" do
      user!("alice")
      time = fn name -> elem(:timer.tc(fn -> Admin.login(name, "nope", %{}) end), 0) end
      _warm = {time.("alice"), time.("mallory")}
      known = Enum.map(1..5, fn _ -> time.("alice") end) |> Enum.sort() |> Enum.at(2)
      unknown = Enum.map(1..5, fn _ -> time.("mallory") end) |> Enum.sort() |> Enum.at(2)
      assert unknown > known / 3, "unknown #{unknown}us vs known #{known}us"
    end

    test "expiry is fixed at login and never extended by use" do
      user!("alice")
      {:ok, token, _} = Admin.login("alice", "correct horse", %{}, 3600)
      s = Repo.one!(Session)
      Admin.session_user(token)
      assert Repo.one!(Session).expires_at == s.expires_at

      Repo.update_all(Session, set: [expires_at: DateTime.add(DateTime.utc_now(), -1, :second)])
      assert Admin.session_user(token) == nil
    end

    test "logout deletes the session row" do
      user!("alice")
      {:ok, token, _} = Admin.login("alice", "correct horse", %{})
      assert :ok = Admin.logout(token)
      assert Admin.session_user(token) == nil
      assert Repo.aggregate(Session, :count) == 0
    end

    test "set_password kills that user's other sessions but keeps the one given" do
      user!("alice")
      {:ok, mine, _} = Admin.login("alice", "correct horse", %{})
      {:ok, other, _} = Admin.login("alice", "correct horse", %{})

      assert :ok = Admin.set_password("alice", "new pw", "new pw", keep: mine)
      assert Admin.session_user(mine)
      assert Admin.session_user(other) == nil
      assert :error = Admin.login("alice", "correct horse", %{})
      assert {:ok, _, _} = Admin.login("alice", "new pw", %{})
      assert {:error, :password_mismatch} = Admin.set_password("alice", "a", "b")
      assert {:error, :not_found} = Admin.set_password("nobody", "a", "a")
    end
  end

  describe "delete_user/2" do
    test "not yourself, not the last admin, and their sessions go with them" do
      user!("alice")
      user!("bob")
      {:ok, bob_token, _} = Admin.login("bob", "correct horse", %{})

      assert {:error, :self} = Admin.delete_user("alice", "alice")
      assert :ok = Admin.delete_user("bob", "alice")
      assert Admin.session_user(bob_token) == nil
      assert {:error, :not_found} = Admin.delete_user("bob", "alice")
      # alice is the last admin now; a delete by someone else (e.g. a stale session) is refused
      assert {:error, :last_admin} = Admin.delete_user("alice", "bob")
      assert Repo.get_by(User, username: "alice")
    end
  end
end
