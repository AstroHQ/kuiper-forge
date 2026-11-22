defmodule KuiperCoordinator.RunHistory.Summary do
  @moduledoc """
  Turns the run history into the numbers the metrics page shows (Rust metrics/summary.rs, parity
  MX6). Plain functions over fetched `RunnerRun`/`JobRun` rows, sorted in memory.

    * success % = completed / (completed + failed + lost + command_failed); `rejected` (agent
      full) and our own teardowns (`idled`, `drained`, `destroyed` = stopped) count for neither
    * nearest-rank percentiles; negative spans (clock skew, events out of order) are dropped
    * steps: `ran` leaves out skipped (0) runs, `typical` counts them as 0
  """

  alias KuiperCoordinator.RunHistory.JobRun

  @hour 3_600_000
  @day 24 * @hour

  # Agents drawn in the per-agent charts, the busiest first.
  @max_chart_agents 8

  @ranges [{"24h", @day}, {"7d", 7 * @day}, {"30d", 30 * @day}, {"90d", 90 * @day}]

  @step_order ~w(create boot install setup job destroy)
  @after_start ~w(job destroy)

  @source_image "image"
  @source_not_recorded "not_recorded"
  @source_order ["hit", "filled", "fell_back", "vm", @source_not_recorded, @source_image]

  @month_abbr ~w(Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec)

  ## ranges

  @doc "The page's ranges, shortest first."
  def ranges, do: Enum.map(@ranges, &elem(&1, 0))

  @doc "A known range, else `\"7d\"`."
  def parse_range(s) when is_binary(s), do: if(s in ranges(), do: s, else: "7d")
  def parse_range(_), do: "7d"

  def range_ms(range), do: @ranges |> List.keyfind(range, 0) |> elem(1)

  @doc "Hourly buckets for the last day, daily past that."
  def bucket_ms("24h"), do: @hour
  def bucket_ms(_), do: @day

  ## stats

  @doc "Count, p50 and p95 of durations in millis."
  def stat(values) do
    sorted = Enum.sort(values)
    %{count: length(sorted), p50: percentile(sorted, 0.50), p95: percentile(sorted, 0.95)}
  end

  @doc "Nearest-rank percentile of sorted values."
  def percentile([], _p), do: nil

  def percentile(sorted, p) do
    n = length(sorted)
    rank = (p * n) |> Float.ceil() |> trunc() |> max(1) |> min(n)
    Enum.at(sorted, rank - 1)
  end

  defp span(nil, _), do: nil
  defp span(_, nil), do: nil
  defp span(from, to) when to - from >= 0, do: to - from
  defp span(_, _), do: nil

  defp spans(pairs), do: for({a, b} <- pairs, d = span(a, b), d != nil, do: d)

  defp failure?(outcome), do: outcome in ~w(failed lost command_failed)

  ## steps

  @doc "Every known step, then any other step a run reported, alphabetically."
  def step_names(runs) do
    extra =
      runs
      |> Enum.flat_map(&Map.keys(&1.steps || %{}))
      |> Enum.map(&to_string/1)
      |> Enum.reject(&(&1 in @step_order))
      |> Enum.uniq()
      |> Enum.sort()

    @step_order ++ extra
  end

  defp empty_step, do: %{ran: stat([]), skipped: 0, typical: stat([])}

  @doc "A group's stats for one step (empty if no run reported it)."
  def step(group, name), do: Map.get(group.steps, name, empty_step())

  @doc "Share of the runs reporting the step that skipped it, nil if none did."
  def skipped_pct(%{skipped: 0}), do: nil
  def skipped_pct(%{skipped: s, typical: %{count: n}}), do: 100.0 * s / n

  ## runner groups

  @doc "Runner stats for every attempt together."
  def total(runs), do: summarize("", "", "", runs)

  @doc "Runner stats per agent, busiest first."
  def by_agent(runs), do: group_runs(runs, &{&1.agent_id, &1.hostname, &1.agent_type})

  @doc "Runner stats per image (older agents: per label set), busiest first."
  def by_image(runs) do
    group_runs(runs, fn r ->
      key = image_key(r)
      {key, key, r.agent_type}
    end)
  end

  defp image_key(%{image: image}) when is_binary(image), do: image
  defp image_key(r), do: "labels: " <> Enum.join(r.labels || [], ", ")

  defp group_runs(runs, key_fun) do
    {order, groups, labels} =
      Enum.reduce(runs, {[], %{}, %{}}, fn run, {order, groups, labels} ->
        {k, label, detail} = key_fun.(run)
        order = if Map.has_key?(groups, k), do: order, else: [k | order]
        # runs are oldest first, so this keeps the newest hostname/agent type
        {order, Map.update(groups, k, [run], &[run | &1]), Map.put(labels, k, {label, detail})}
      end)

    order
    |> Enum.map(fn k ->
      {label, detail} = labels[k]
      summarize(k, label, detail, Enum.reverse(groups[k]))
    end)
    |> Enum.sort_by(&{-&1.runs, &1.label})
  end

  defp summarize(key, label, detail, runs) do
    counts =
      Enum.reduce(
        runs,
        %{running: 0, completed: 0, rejected: 0, stopped: 0, failures: 0, phases: %{}},
        fn run, acc ->
          case run.outcome do
            nil ->
              %{acc | running: acc.running + 1}

            "completed" ->
              %{acc | completed: acc.completed + 1}

            "rejected" ->
              %{acc | rejected: acc.rejected + 1}

            o when o in ~w(idled drained destroyed) ->
              %{acc | stopped: acc.stopped + 1}

            o ->
              if failure?(o) do
                phase = run.failed_phase || "unknown"

                %{
                  acc
                  | failures: acc.failures + 1,
                    phases: Map.update(acc.phases, phase, 1, &(&1 + 1))
                }
              else
                acc
              end
          end
        end
      )

    decided = counts.completed + counts.failures

    %{
      key: key,
      label: label,
      detail: detail,
      runs: length(runs),
      running: counts.running,
      completed: counts.completed,
      failures: counts.failures,
      rejected: counts.rejected,
      stopped: counts.stopped,
      success_pct: if(decided > 0, do: 100.0 * counts.completed / decided),
      ack: stat(spans(for r <- runs, do: {r.requested_at, r.acked_at})),
      provision: stat(spans(for r <- runs, do: {r.requested_at, r.started_at})),
      steps: step_stats(runs),
      failed_phases: Enum.sort_by(counts.phases, fn {p, n} -> {-n, p} end)
    }
  end

  defp step_stats(runs) do
    runs
    |> Enum.flat_map(fn r -> Enum.map(r.steps || %{}, fn {s, ms} -> {to_string(s), ms} end) end)
    |> Enum.group_by(&elem(&1, 0), &elem(&1, 1))
    |> Map.new(fn {step, values} ->
      ran = Enum.filter(values, &(&1 > 0))

      {step, %{ran: stat(ran), skipped: length(values) - length(ran), typical: stat(values)}}
    end)
  end

  ## installs

  defp install_source(%{install_source: s}) when is_binary(s), do: s

  defp install_source(r) do
    case Map.get(r.steps || %{}, "install") do
      nil -> nil
      0 -> @source_image
      _ -> @source_not_recorded
    end
  end

  defp source_rank(source),
    do: Enum.find_index(@source_order, &(&1 == source)) || length(@source_order)

  @doc """
  Runner installs per image and source: busiest image first, then by source order. Failed
  installs count in `runs` but have no time.
  """
  def by_install(runs) do
    rows =
      for r <- runs, source = install_source(r), source != nil do
        ms = Map.get(r.steps || %{}, "install")
        {image_key(r), source, if(is_integer(ms) and ms > 0, do: ms)}
      end

    per_image = Enum.frequencies_by(rows, &elem(&1, 0))

    rows
    |> Enum.group_by(fn {image, source, _} -> {image, source} end, &elem(&1, 2))
    |> Enum.map(fn {{image, source}, times} ->
      %{
        image: image,
        source: source,
        runs: length(times),
        install: stat(Enum.reject(times, &is_nil/1))
      }
    end)
    |> Enum.sort_by(&{-per_image[&1.image], &1.image, source_rank(&1.source), &1.source})
  end

  ## jobs

  @doc "Where jobs' time in the queue went."
  def jobs(jobs, runs) do
    by_name = Map.new(runs, &{&1.runner_name, &1})

    # polling mode never hears which runner took a job: use the last one that completed it
    completed_by_job =
      for r <- runs, r.job_key != nil, r.outcome == "completed", into: %{}, do: {r.job_key, r}

    {counts, attempts} =
      Enum.reduce(
        jobs,
        {%{pending: 0, ran: 0, abandoned: 0, gone: 0, cancelled: 0}, 0},
        fn %JobRun{} = job, {c, a} ->
          c =
            case job.outcome do
              nil ->
                Map.update!(c, :pending, &(&1 + 1))

              o when o in ~w(ran abandoned gone cancelled) ->
                Map.update!(c, String.to_atom(o), &(&1 + 1))

              _ ->
                c
            end

          {c, a + (job.attempts || 0)}
        end
      )

    ran_by =
      for job <- jobs,
          run = (job.runner_name && by_name[job.runner_name]) || completed_by_job[job.job_key],
          run != nil,
          do: {job, run}

    Map.merge(counts, %{
      jobs: length(jobs),
      mean_attempts: if(jobs != [], do: attempts / length(jobs)),
      wait: stat(spans(for j <- jobs, do: {j.queued_at, j.started_at})),
      # equal means the forge gave no created time, not that it took no time
      to_coordinator: stat(for j <- jobs, j.queued_at < j.seen_at, do: j.seen_at - j.queued_at),
      dispatch: stat(spans(for {j, r} <- ran_by, do: {j.seen_at, r.requested_at})),
      provision: stat(spans(for {_, r} <- ran_by, do: {r.requested_at, r.started_at})),
      pickup: stat(spans(for {j, r} <- ran_by, do: {r.started_at, j.started_at}))
    })
  end

  ## charts

  @doc "Bucket starts covering `since` to `now`, aligned to the bucket size (UTC)."
  def bucket_starts(since, now, size) do
    first = Integer.floor_div(since, size) * size
    first |> Stream.iterate(&(&1 + size)) |> Enum.take_while(&(&1 <= now))
  end

  defp bucket_label(start, size) do
    t = DateTime.from_unix!(start, :millisecond)
    date = "#{Enum.at(@month_abbr, t.month - 1)} #{pad(t.day)}"
    if size < @day, do: "#{date} #{pad(t.hour)}:00", else: date
  end

  defp pad(n), do: n |> Integer.to_string() |> String.pad_leading(2, "0")

  defp secs(nil), do: nil
  defp secs(ms), do: Float.round(ms / 1000, 1)

  @doc """
  Series for the charts. Durations are seconds, gaps nil: outcomes per bucket, p95 time to a
  running runner per agent, p50/p95 queue wait, and p50 per step before the runner is up per agent.
  """
  def charts(runs, jobs, agents, steps, range, now) do
    size = bucket_ms(range)
    starts = bucket_starts(now - range_ms(range), now, size)
    n = length(starts)
    first = List.first(starts, now)

    index = fn t ->
      i = Integer.floor_div(t - first, size)
      if i >= 0 and i < n, do: i
    end

    chart_agents = Enum.take(agents, @max_chart_agents)
    chart_keys = MapSet.new(chart_agents, & &1.key)

    bucketed = for r <- runs, i = index.(r.requested_at), i != nil, do: {i, r}

    outcome_kind = fn
      "completed" -> :completed
      "rejected" -> :rejected
      o -> if failure?(o), do: :failed, else: :other
    end

    counts =
      bucketed
      |> Enum.reject(fn {_, r} -> is_nil(r.outcome) end)
      |> Enum.frequencies_by(fn {i, r} -> {outcome_kind.(r.outcome), i} end)

    series = fn kind -> for i <- 0..(n - 1)//1, do: Map.get(counts, {kind, i}, 0) end

    provision =
      for {i, r} <- bucketed,
          r.agent_id in chart_keys,
          d = span(r.requested_at, r.started_at),
          d != nil,
          reduce: %{} do
        acc -> Map.update(acc, {r.agent_id, i}, [d], &[d | &1])
      end

    waits =
      for j <- jobs,
          i = index.(j.queued_at),
          i != nil,
          d = span(j.queued_at, j.started_at),
          d != nil,
          reduce: %{} do
        acc -> Map.update(acc, i, [d], &[d | &1])
      end

    wait_stats = for i <- 0..(n - 1)//1, do: stat(Map.get(waits, i, []))

    %{
      buckets: Enum.map(starts, &bucket_label(&1, size)),
      outcomes: %{
        completed: series.(:completed),
        failed: series.(:failed),
        rejected: series.(:rejected),
        other: series.(:other)
      },
      provision:
        for g <- chart_agents do
          %{
            label: g.label,
            data: for(i <- 0..(n - 1)//1, do: secs(stat(Map.get(provision, {g.key, i}, [])).p95))
          }
        end,
      wait_p50: Enum.map(wait_stats, &secs(&1.p50)),
      wait_p95: Enum.map(wait_stats, &secs(&1.p95)),
      phases: %{
        agents: Enum.map(chart_agents, & &1.label),
        steps:
          for s <- steps, s not in @after_start do
            %{label: s, data: Enum.map(chart_agents, &secs(step(&1, s).typical.p50))}
          end
      }
    }
  end

  ## formatting

  @doc "Short duration for tables: `850ms`, `42.3s`, `3m 12s`, `2h 5m`."
  def fmt_ms(nil), do: "-"
  def fmt_ms(ms) when ms < 1_000, do: "#{ms}ms"

  def fmt_ms(ms) do
    s = div(ms, 1_000)

    cond do
      s < 60 -> :erlang.float_to_binary(ms / 1_000, decimals: 1) <> "s"
      s < 3_600 -> "#{div(s, 60)}m #{rem(s, 60)}s"
      true -> "#{div(s, 3_600)}h #{div(rem(s, 3_600), 60)}m"
    end
  end
end
