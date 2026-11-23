defmodule Mix.Tasks.Kuiper.ImportRust do
  @shortdoc "Imports agents, tokens, admins and forges from a Rust coordinator"
  @moduledoc """
  Copies a Rust coordinator's agents (revocations kept), unexpired registration tokens, admin
  users, API tokens and, with `--config`, its forges, provisioning settings and label mappings
  into this app's database (`KuiperCoordinator.RustImport`).

      mix kuiper.import_rust --from sqlite:///var/lib/kuiper/kuiper.db --config /etc/kuiper/config.toml --dry-run

  `--from` takes `sqlite://PATH` or `postgres://USER:PASS@HOST/DB`; the database is only read.
  `--dry-run` prints what would change and writes nothing. Rerunning is safe: what's already
  here is left alone and differences are reported as conflicts. In a release, use
  `KuiperCoordinator.Release.import_rust/2`.
  """
  use Mix.Task

  @requirements ["app.config"]

  @impl Mix.Task
  def run(args) do
    {opts, rest, invalid} =
      OptionParser.parse(args, strict: [from: :string, config: :string, dry_run: :boolean])

    cond do
      invalid != [] or rest != [] ->
        Mix.raise(
          "unknown arguments: #{Enum.map_join(invalid, " ", &elem(&1, 0))} #{Enum.join(rest, " ")}"
        )

      opts[:from] in [nil, ""] ->
        Mix.raise(
          "give the Rust coordinator's database with --from sqlite://PATH or postgres://..."
        )

      true ->
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
