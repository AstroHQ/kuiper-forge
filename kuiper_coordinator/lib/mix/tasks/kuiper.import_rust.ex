defmodule Mix.Tasks.Kuiper.ImportRust do
  @shortdoc "Imports agents, tokens, admins and forges from a Rust coordinator"
  @moduledoc """
  Copies a Rust coordinator's agents (revocations kept), unexpired registration tokens, admin
  users, API tokens and, with `--config`, its forges, provisioning settings and label mappings
  into this app's database (`KuiperCoordinator.RustImport`).

      mix kuiper.import_rust --sqlite /var/lib/kuiper/kuiper.db --config /etc/kuiper/config.toml --dry-run

  `--sqlite PATH`, or `--from` with `sqlite://PATH` or `postgres://USER:PASS@HOST/DB`, names the
  Rust database; it's only read. The Rust `KUIPER_X__Y` environment overrides apply on top of
  `--config` the way the Rust coordinator applies them, and without `--config` the environment
  alone is the forge config (env-configured installs).
  `--dry-run` prints what would change and writes nothing. Rerunning is safe: what's already
  here is left alone and differences are reported as conflicts. In a release, run
  `bin/import_rust` with the same flags.
  """
  use Mix.Task

  @requirements ["app.config"]

  @impl Mix.Task
  def run(args) do
    case KuiperCoordinator.RustImport.parse_args(args) do
      {:error, msg} ->
        Mix.raise(msg)

      {:ok, opts} ->
        {:ok, _} = Application.ensure_all_started([:ssl, :postgrex, :ecto_sql])
        repo = KuiperCoordinator.Repo

        # in tests the app (and the repo) is already running
        if Process.whereis(repo) do
          import_rust(opts)
        else
          {:ok, _, _} = Ecto.Migrator.with_repo(repo, fn _ -> import_rust(opts) end)
        end
    end
  end

  defp import_rust(opts) do
    report = KuiperCoordinator.RustImport.cli(opts, &Mix.raise/1)

    if KuiperCoordinator.RustImport.conflicts?(report),
      do: Mix.shell().info("some rows conflicted and were left as Phoenix has them (see above)")

    report
  end
end
