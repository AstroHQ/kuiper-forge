defmodule KuiperCoordinator.Admin do
  @moduledoc """
  Admin UI users and sessions (Rust `admin/auth.rs`, parity UI1, UI8, UI10).

  Passwords are Argon2id PHC hashes. A login makes a 64-char alphanumeric session id; only its
  SHA-256 is stored. Sessions expire `session_timeout_secs` (default 3600) after login and are
  never extended. A login for an unknown user still runs a hash, so timing doesn't tell which
  names exist.

  Always on (D49: a web app always has its UI); login is still required.

  Settings: `config :kuiper_coordinator, :admin, session_timeout_secs: int, coordinator_url: url,
  update_check: bool`.
  """
  import Ecto.Query

  alias KuiperCoordinator.Repo
  alias KuiperCoordinator.Admin.{Session, User}

  @name_re ~r/\A[A-Za-z0-9._\-@]{1,64}\z/

  def settings, do: Application.get_env(:kuiper_coordinator, :admin, [])
  def session_timeout, do: Keyword.get(settings(), :session_timeout_secs, 3600)

  @doc "Agent-facing URL put in registration bundles (Rust [admin] coordinator_url)."
  def coordinator_url, do: Keyword.get(settings(), :coordinator_url, "https://localhost:9543")

  ## Users

  def list_users, do: Repo.all(from(u in User, order_by: u.username))

  def get_user(username), do: Repo.get_by(User, username: username)

  def user_count, do: Repo.aggregate(User, :count)

  @setup_lock 0x6B75_6970_6572_0001

  @doc """
  Creates a user. The name is trimmed, then must be 1-64 of `[A-Za-z0-9._-@]` (safe in HTML,
  URLs and logs, user_routes.rs); the password must be non-empty and match `confirm`.

  Every user insert (this, the CLI's `Release.create_admin/1`, and `create_first_user/3`) runs
  under the first-user advisory lock, so a first-run setup can't miss a create that's still
  uncommitted and make a second "first" admin.
  """
  def create_user(username, password, confirm) do
    with {:ok, username, hash} <- prepare_user(username, password, confirm) do
      Repo.transaction(fn ->
        take_setup_lock()
        insert_user(username, hash)
      end)
    end
  end

  @doc """
  First-run setup (D50): creates `username` only while there are no users. The count and the
  insert run in one transaction under the same advisory lock as `create_user/3`, so racing
  setups (or a setup racing the CLI) make exactly one first admin; the rest get
  `{:error, :setup_done}`.
  """
  def create_first_user(username, password, confirm) do
    with {:ok, username, hash} <- prepare_user(username, password, confirm) do
      Repo.transaction(fn ->
        take_setup_lock()
        if user_count() > 0, do: Repo.rollback(:setup_done), else: insert_user(username, hash)
      end)
    end
  end

  # Validates, then hashes outside the lock (Argon2 is slow). Non-string input is refused, not
  # crashed on: the setup form is unauthenticated.
  defp prepare_user(username, password, confirm) do
    username = if is_binary(username), do: String.trim(username), else: username

    with :ok <- check_name(username),
         :ok <- check_password(password, confirm) do
      {:ok, username, Argon2.hash_pwd_salt(password)}
    end
  end

  defp take_setup_lock, do: Repo.query!("SELECT pg_advisory_xact_lock($1)", [@setup_lock])

  defp insert_user(username, hash) do
    %User{username: username, password_hash: hash}
    |> Ecto.Changeset.change()
    |> Ecto.Changeset.unique_constraint(:username)
    |> Repo.insert()
    |> case do
      {:ok, user} -> user
      {:error, %Ecto.Changeset{}} -> Repo.rollback(:username_taken)
    end
  end

  @doc "True while no admin user exists (the first-run setup page is open)."
  def setup_needed?, do: not Repo.exists?(User)

  @doc "A user-facing message for a `create_user/3` or `set_password/4` error."
  def error_message(:invalid_username),
    do: "Usernames are 1-64 characters: letters, digits, and . _ - @"

  def error_message(:empty_password), do: "Password can't be empty"
  def error_message(:password_mismatch), do: "Passwords don't match"
  def error_message(:username_taken), do: "That username already exists"

  defp check_name(name) when is_binary(name),
    do: if(name =~ @name_re, do: :ok, else: {:error, :invalid_username})

  defp check_name(_), do: {:error, :invalid_username}

  defp check_password(pw, _) when pw in [nil, ""], do: {:error, :empty_password}
  defp check_password(pw, _) when not is_binary(pw), do: {:error, :empty_password}
  defp check_password(pw, pw), do: :ok
  defp check_password(_, _), do: {:error, :password_mismatch}

  @doc """
  Sets a user's password and ends their sessions, except the session id in `keep:` (yours,
  when you change your own password).
  """
  def set_password(username, password, confirm, opts \\ []) do
    with :ok <- check_password(password, confirm),
         %User{} = user <- get_user(username) || {:error, :not_found} do
      keep = opts[:keep] && hash_token(opts[:keep])

      Repo.transaction(fn ->
        user
        |> Ecto.Changeset.change(password_hash: Argon2.hash_pwd_salt(password))
        |> Repo.update!()

        q = from(s in Session, where: s.user_id == ^user.id)
        q = if keep, do: from(s in q, where: s.token_hash != ^keep), else: q
        Repo.delete_all(q)
      end)

      :ok
    end
  end

  @doc """
  Deletes `username` on behalf of `actor`. Not yourself, and never the last admin: the count
  check and the delete share a transaction that locks every admin row, so two admins deleting
  each other at once can't both succeed (#32, 176390d). Their sessions go with them.
  """
  def delete_user(username, actor) when username == actor, do: {:error, :self}

  def delete_user(username, _actor) do
    Repo.transaction(fn ->
      _locked = Repo.all(from(u in User, select: u.id, lock: "FOR UPDATE"))

      case Repo.delete_all(from(u in User, where: u.username == ^username)) do
        {0, _} ->
          Repo.rollback(:not_found)

        {1, _} ->
          if Repo.aggregate(User, :count) == 0, do: Repo.rollback(:last_admin), else: :ok
      end
    end)
    |> case do
      {:ok, :ok} -> :ok
      {:error, reason} -> {:error, reason}
    end
  end

  ## Sessions

  @doc """
  Checks the password and starts a session: `{:ok, session_id, user}` or `:error`. `meta` may
  carry `:ip_address` and `:user_agent` for the session row.
  """
  def login(username, password, meta, timeout \\ session_timeout()) do
    case get_user(String.trim(username || "")) do
      %User{} = user ->
        if Argon2.verify_pass(password || "", user.password_hash) do
          {:ok, start_session(user, meta, timeout), touch_login(user)}
        else
          :error
        end

      nil ->
        # same work as a real check (UI10)
        Argon2.no_user_verify()
        :error
    end
  end

  defp start_session(user, meta, timeout) do
    token = random_token()

    Repo.insert!(%Session{
      user_id: user.id,
      token_hash: hash_token(token),
      expires_at: DateTime.add(DateTime.utc_now(), timeout, :second),
      ip_address: meta[:ip_address],
      user_agent: meta[:user_agent] && String.slice(meta[:user_agent], 0, 512)
    })

    token
  end

  defp touch_login(user) do
    user |> Ecto.Changeset.change(last_login_at: DateTime.utc_now()) |> Repo.update!()
  end

  @doc "The user for a live session id, or nil (unknown or expired). Doesn't extend it."
  def session_user(token) when is_binary(token) do
    now = DateTime.utc_now()

    Repo.one(
      from(s in Session,
        join: u in assoc(s, :user),
        where: s.token_hash == ^hash_token(token) and s.expires_at > ^now,
        select: u
      )
    )
  end

  def session_user(_), do: nil

  @doc "Ends a session (logout deletes the row, UI1)."
  def logout(token) when is_binary(token) do
    Repo.delete_all(from(s in Session, where: s.token_hash == ^hash_token(token)))
    :ok
  end

  def logout(_), do: :ok

  @doc "Drops expired sessions."
  def purge_sessions do
    now = DateTime.utc_now()
    {n, _} = Repo.delete_all(from(s in Session, where: s.expires_at <= ^now))
    n
  end

  defp hash_token(token), do: :crypto.hash(:sha256, token)

  # 64 alphanumerics, like Rust generate_session_id
  defp random_token, do: KuiperCoordinator.CA.random_string(64)
end
