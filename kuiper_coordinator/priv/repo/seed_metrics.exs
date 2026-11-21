# Mock runner and job history for working on the metrics page (like the Rust coordinator's
# examples/seed_metrics.rs):
#
#     mix run priv/repo/seed_metrics.exs            # 30 days, ending now
#     mix run priv/repo/seed_metrics.exs --days 7
#     mix run priv/repo/seed_metrics.exs --clear    # remove the mock rows again
#
# Everything goes under the `mock` forge, so a re-run or --clear only touches mock rows. The
# mock agents aren't registered, so their links on the page 404.
import Ecto.Query
alias KuiperCoordinator.{Repo, RunHistory}
alias KuiperCoordinator.RunHistory.{JobRun, RunnerRun}

{opts, _, _} = OptionParser.parse(System.argv(), strict: [days: :integer, clear: :boolean])
Repo.delete_all(from(r in RunnerRun, where: r.forge == "mock"))
Repo.delete_all(from(j in JobRun, where: j.forge == "mock"))

unless opts[:clear] do
  :rand.seed(:exsss, {1, 2, 3})
  hour = 3_600_000
  now = System.system_time(:millisecond)
  days = opts[:days] || 30

  agents = [
    {"mock_mac_studio", "mac-studio.local", "tart", "macos-15-xcode-26", 0.97, 25_000},
    {"mock_mac_mini", "mac-mini.local", "tart", "macos-15-xcode-26", 0.88, 40_000},
    {"mock_pve", "pve1.lan", "proxmox", "ubuntu-24.04", 0.99, 15_000},
    {"mock_kvm", "kvm.lan", "libvirt", "windows-11", 0.92, 60_000}
  ]

  jitter = fn ms -> round(ms * (0.6 + :rand.uniform() * 0.9)) end

  for n <- 1..(days * 40) do
    queued = now - round(:rand.uniform() * days * 24 * hour)
    seen = queued + jitter.(1_500)
    key = "mock:#{n}"

    {agent_id, host, type, image, ok_rate, boot} = Enum.random(agents)

    {runs, ran?} =
      Enum.reduce_while(1..3, {[], false}, fn attempt, {acc, _} ->
        requested = seen + jitter.(4_000) + (attempt - 1) * 120_000
        steps = %{
          "create" => jitter.(6_000),
          "boot" => jitter.(boot),
          "install" => if(:rand.uniform() < 0.7, do: 0, else: jitter.(30_000)),
          "setup" => jitter.(8_000)
        }

        up = requested + 1_000 + Enum.sum(Map.values(steps))
        ok? = :rand.uniform() < ok_rate

        run = %{
          runner_name: "mock-#{n}-#{attempt}",
          agent_id: agent_id,
          hostname: host,
          agent_type: type,
          forge: "mock",
          job_key: key,
          image: image,
          labels: [type],
          requested_at: requested,
          acked_at: requested + jitter.(300),
          started_at: if(ok?, do: up),
          job_started_at: if(ok?, do: up + 2_000),
          finished_at: up + 300_000,
          outcome: if(ok?, do: "completed", else: Enum.random(~w(failed lost))),
          failed_phase: if(!ok?, do: Enum.random(~w(boot install setup))),
          install_source: if(steps["install"] > 0, do: Enum.random(~w(hit hit filled fell_back))),
          steps: if(ok?, do: Map.put(steps, "job", jitter.(240_000)), else: steps)
        }

        if ok?, do: {:halt, {[run | acc], true}}, else: {:cont, {[run | acc], false}}
      end)

    Enum.each(runs, &RunHistory.insert_run!/1)
    last = hd(runs)

    RunHistory.insert_job!(%{
      job_key: key,
      forge: "mock",
      repository: "astro/app",
      job_name: "build",
      queued_at: queued,
      seen_at: seen,
      started_at: if(ran?, do: last.job_started_at),
      runner_name: if(ran?, do: last.runner_name),
      attempts: length(runs),
      outcome: if(ran?, do: "ran", else: "abandoned")
    })
  end

  IO.puts("Seeded #{days} days of mock run history")
end
