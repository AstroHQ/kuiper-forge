defmodule KuiperCoordinator.Release do
  @moduledoc """
  Release tasks, run with `bin/kuiper_coordinator eval '...'` (parity C9: the Rust CLI's
  `admin create-user`, `list-users`, `reset-password`). Passwords come from
  KUIPER_ADMIN_PASSWORD, never the command line.
  """
  @app :kuiper_coordinator

  def migrate do
    load_app()

    for repo <- repos() do
      {:ok, _, _} = Ecto.Migrator.with_repo(repo, &Ecto.Migrator.run(&1, :up, all: true))
    end
  end

  @doc "Creates each repo's database if it's missing (`bin/kuiper_coordinator eval`)."
  def create_db do
    load_app()

    for repo <- repos() do
      case repo.__adapter__().storage_up(repo.config()) do
        :ok -> :ok
        {:error, :already_up} -> :ok
        {:error, e} -> raise "could not create the #{inspect(repo)} database: #{inspect(e)}"
      end
    end
  end

  @doc "Drops each repo's database (the release smoke test cleans up with it)."
  def drop_db do
    load_app()
    for repo <- repos(), do: repo.__adapter__().storage_down(repo.config())
  end

  def rollback(repo, version) do
    load_app()
    {:ok, _, _} = Ecto.Migrator.with_repo(repo, &Ecto.Migrator.run(&1, :down, to: version))
  end

  @doc "Creates an admin user with KUIPER_ADMIN_PASSWORD."
  def create_admin(username) do
    with_repo(fn ->
      pw = password!()

      case KuiperCoordinator.Admin.create_user(username, pw, pw) do
        {:ok, user} -> IO.puts("created admin user #{user.username}")
        {:error, reason} -> fail("cannot create admin user: #{reason}")
      end
    end)
  end

  @doc "Sets a user's password from KUIPER_ADMIN_PASSWORD and ends all their sessions."
  def reset_admin_password(username) do
    with_repo(fn ->
      pw = password!()

      case KuiperCoordinator.Admin.set_password(username, pw, pw) do
        :ok -> IO.puts("password reset for #{username}; their sessions are logged out")
        {:error, reason} -> fail("cannot reset password: #{reason}")
      end
    end)
  end

  def list_admins do
    with_repo(fn ->
      for u <- KuiperCoordinator.Admin.list_users() do
        IO.puts("#{u.username}\tlast login #{u.last_login_at || "never"}")
      end
    end)
  end

  @doc """
  Imports agents, tokens, admin users, API tokens and (with `config:`) forges from a Rust
  coordinator (`KuiperCoordinator.RustImport`):

      bin/kuiper_coordinator eval 'KuiperCoordinator.Release.import_rust("sqlite:///var/lib/kuiper/kuiper.db", config: "/etc/kuiper/config.toml", dry_run: true)'

  Exits 1 on an error, 2 when something conflicted (the rest is imported, conflicts are left
  as Phoenix has them).
  """
  def import_rust(from, opts \\ []) do
    with_repo(fn ->
      report = KuiperCoordinator.RustImport.cli([{:from, from} | opts], &fail/1)
      if KuiperCoordinator.RustImport.conflicts?(report), do: System.halt(2)
    end)
  end

  defp password! do
    case System.get_env("KUIPER_ADMIN_PASSWORD") do
      pw when pw in [nil, ""] -> fail("set KUIPER_ADMIN_PASSWORD")
      pw -> pw
    end
  end

  defp fail(msg) do
    IO.puts(:stderr, msg)
    System.halt(1)
  end

  defp with_repo(fun) do
    load_app()
    {:ok, _, _} = Ecto.Migrator.with_repo(KuiperCoordinator.Repo, fn _ -> fun.() end)
    :ok
  end

  defp repos do
    Application.fetch_env!(@app, :ecto_repos)
  end

  defp load_app do
    Application.ensure_all_started(:ssl)
    Application.load(@app)
  end
end
