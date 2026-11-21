defmodule KuiperCoordinatorWeb.Admin.MetricsLive do
  @moduledoc """
  Metrics (parity UI6, #67; Rust admin/metrics_routes.rs + metrics.html): range tabs
  24h/7d/30d/90d (unknown -> 7d), header stats, outcome/provision/step/wait charts (Chart.js
  via the `MetricsCharts` hook, data in an escaped attribute), per-agent and per-image tables,
  runner installs and the job wait breakdown. Also the agent page's 7-day card.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.RunHistory
  alias KuiperCoordinator.RunHistory.Summary

  @week_ms 7 * 86_400_000

  @impl true
  def mount(_params, _session, socket), do: {:ok, assign(socket, page_title: "Metrics")}

  @impl true
  def handle_params(params, _uri, socket) do
    range = Summary.parse_range(params["range"])
    now = System.system_time(:millisecond)
    since = now - Summary.range_ms(range)
    runs = RunHistory.runs_since(since)
    jobs = RunHistory.jobs_since(since)

    agents = Summary.by_agent(runs)
    steps = Summary.step_names(runs)

    {:noreply,
     assign(socket,
       range: range,
       runs: runs,
       all: Summary.total(runs),
       agents: agents,
       images: Summary.by_image(runs),
       installs: Summary.by_install(runs),
       jobs: Summary.jobs(jobs, runs),
       steps: steps,
       charts: Jason.encode!(Summary.charts(runs, jobs, agents, steps, range, now))
     )}
  end

  ## views shared with the agent page

  @doc "The agent page's card: its runners over the last week, nil when it ran none."
  def agent_performance(agent_id) do
    since = System.system_time(:millisecond) - @week_ms

    since
    |> RunHistory.runs_since()
    |> Enum.filter(&(&1.agent_id == agent_id))
    |> Summary.by_agent()
    |> List.first()
  end

  @doc "ok >= 95%, warn >= 80%, else bad; idle before anything finished."
  def success_status(nil), do: :idle
  def success_status(p) when p >= 95.0, do: :ok
  def success_status(p) when p >= 80.0, do: :warn
  def success_status(_), do: :bad

  def pct(nil), do: "-"
  def pct(p), do: :erlang.float_to_binary(p * 1.0, decimals: 1) <> "%"

  def failed_phases(g), do: Enum.map_join(g.failed_phases, ", ", fn {p, n} -> "#{p} #{n}" end)

  defp badge_class(:ok), do: "badge-success"
  defp badge_class(:warn), do: "badge-warning"
  defp badge_class(:bad), do: "badge-error"
  defp badge_class(:idle), do: "badge-ghost"

  defp step_title("create"), do: "clone the VM"
  defp step_title("boot"), do: "start the VM until SSH answers"

  defp step_title("install"),
    do: "get the runner onto the VM, skipped when the image already has it"

  defp step_title("setup"),
    do: "the rest of getting the runner ready: SSH, clock sync, runner config"

  defp step_title("job"), do: "the runner's run"
  defp step_title("destroy"), do: "stop and delete the VM"
  defp step_title(_), do: "reported by newer agents"

  @doc "How an install source reads on the page, and its tooltip."
  def install_source_view("hit"), do: {"Cache hit", "uploaded from the agent's copy"}

  def install_source_view("filled"),
    do: {"Filled cache", "uploaded from the agent's copy, after waiting for it to download"}

  def install_source_view("fell_back"),
    do:
      {"Fell back", "the cache didn't work out so the VM downloaded it, the agent's log says why"}

  def install_source_view("vm"), do: {"VM download", "the agent's cache is turned off"}

  def install_source_view("not_recorded"),
    do: {"Not recorded", "from before agents reported this, or an older agent"}

  def install_source_view("image"), do: {"In image", "the image already has the runner"}
  def install_source_view(other), do: {other, "reported by newer agents"}

  ## components

  attr :status, :atom, required: true
  attr :value, :string, required: true

  def success_badge(assigns) do
    ~H"""
    <span class={["badge badge-outline badge-sm", badge_class(@status)]}>{@value}</span>
    """
  end

  attr :s, :map, required: true

  def stat(assigns) do
    ~H"""
    <span :if={@s.count == 0} class="opacity-60">-</span>
    <span :if={@s.count > 0}>
      {Summary.fmt_ms(@s.p50)}
      <div class="text-xs opacity-60" title={"p95, from #{@s.count} runs"}>
        p95 {Summary.fmt_ms(@s.p95)}
      </div>
    </span>
    """
  end

  attr :s, :map, required: true

  defp step_cell(assigns) do
    assigns = assign(assigns, skipped: Summary.skipped_pct(assigns.s))

    ~H"""
    <span :if={@s.ran.count == 0 and is_nil(@skipped)} class="opacity-60">-</span>
    <span :if={@s.ran.count > 0 or @skipped}>
      <.stat s={@s.ran} />
      <div :if={@skipped} class="text-xs opacity-60" title="runs that didn't need this step">
        {round(@skipped)}% skipped
      </div>
    </span>
    """
  end

  attr :id, :string, required: true
  attr :groups, :list, required: true
  attr :steps, :list, required: true
  attr :first, :string, required: true
  attr :link, :boolean, default: false

  defp group_table(assigns) do
    ~H"""
    <div class="overflow-x-auto">
      <table class="table table-sm" id={@id}>
        <thead>
          <tr>
            <th>{@first}</th>
            <th>Runs</th>
            <th title="completed / (completed + failed, lost or no answer). Rejections and our own teardowns don't count">
              Success
            </th>
            <th>Failures</th>
            <th title="CreateRunner sent until the runner is up, by the coordinator's clock">
              To runner up
            </th>
            <th title="agent answered the CreateRunner">Ack</th>
            <th :for={s <- @steps} title={step_title(s)} class="capitalize">{s}</th>
          </tr>
        </thead>
        <tbody>
          <tr :for={g <- @groups}>
            <td>
              <.link :if={@link} navigate={~p"/admin/agents/#{g.key}"} class="link">
                {if g.label == "", do: g.key, else: g.label}
              </.link>
              <code :if={!@link}>{g.label}</code>
              <div :if={g.detail != ""} class="text-xs opacity-60">{g.detail}</div>
            </td>
            <td class="whitespace-nowrap">
              {g.runs}
              <div :if={g.running > 0} class="text-xs opacity-60">{g.running} running</div>
              <div :if={g.rejected > 0} class="text-xs opacity-60" title="agent was full">
                {g.rejected} rejected
              </div>
              <div
                :if={g.stopped > 0}
                class="text-xs opacity-60"
                title="idled, drained or pool runners we destroyed"
              >
                {g.stopped} stopped
              </div>
            </td>
            <td>
              <.success_badge status={success_status(g.success_pct)} value={pct(g.success_pct)} />
            </td>
            <td>
              {g.failures}
              <div :if={g.failed_phases != []} class="text-xs opacity-60">{failed_phases(g)}</div>
            </td>
            <td class="whitespace-nowrap"><.stat s={g.provision} /></td>
            <td class="whitespace-nowrap"><.stat s={g.ack} /></td>
            <td :for={s <- @steps} class="whitespace-nowrap">
              <.step_cell s={Summary.step(g, s)} />
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    """
  end

  attr :g, :map, required: true

  @doc "The agent page's Last 7 days card."
  def performance_card(assigns) do
    assigns =
      assign(assigns,
        install: Summary.step(assigns.g, "install"),
        boot: Summary.step(assigns.g, "boot"),
        job: Summary.step(assigns.g, "job")
      )

    ~H"""
    <section class="card bg-base-200" id="performance">
      <div class="card-body">
        <h2 class="card-title">
          Last 7 days
          <.link navigate={~p"/admin/metrics"} class="link text-sm font-normal">all agents</.link>
        </h2>
        <p class="text-sm opacity-60">p50, with p95 below.</p>
        <dl class="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-2 text-sm">
          <dt class="opacity-60">Runners</dt>
          <dd>{@g.runs}</dd>
          <dt class="opacity-60">Success</dt>
          <dd>
            <.success_badge status={success_status(@g.success_pct)} value={pct(@g.success_pct)} />
            <div :if={@g.failures > 0} class="text-xs opacity-60">
              {@g.failures} failed{if @g.failed_phases != [], do: ": " <> failed_phases(@g)}
            </div>
          </dd>
          <dt class="opacity-60">Time to runner</dt>
          <dd><.stat s={@g.provision} /></dd>
          <dt class="opacity-60">Boot</dt>
          <dd><.stat s={@boot.ran} /></dd>
          <dt class="opacity-60">Runner install</dt>
          <dd>
            <.stat s={@install.ran} />
            <div :if={Summary.skipped_pct(@install)} class="text-xs opacity-60">
              {round(Summary.skipped_pct(@install))}% skipped, already in the image
            </div>
          </dd>
          <dt class="opacity-60">Job</dt>
          <dd><.stat s={@job.ran} /></dd>
        </dl>
      </div>
    </section>
    """
  end

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:metrics}>
      <.header>
        Metrics
        <:subtitle>Runner and job history, UTC.</:subtitle>
        <:actions>
          <div role="tablist" class="tabs tabs-box tabs-sm">
            <.link
              :for={r <- Summary.ranges()}
              patch={~p"/admin/metrics?range=#{r}"}
              role="tab"
              class={["tab", r == @range && "tab-active"]}
              aria-selected={to_string(r == @range)}
            >
              {r}
            </.link>
          </div>
        </:actions>
      </.header>

      <p :if={@runs == []} class="opacity-60" id="metrics-empty">
        No runners in the last {@range}. History starts from when this coordinator was deployed.
      </p>

      <div :if={@runs != []} class="grid grid-cols-2 lg:grid-cols-4 gap-4">
        <div class="card bg-base-200" id="stat-runs">
          <div class="card-body p-4">
            <div class="text-sm opacity-60">Runner attempts</div>
            <div class="text-2xl font-semibold">{@all.runs}</div>
            <div :if={@all.running > 0} class="text-xs opacity-60">{@all.running} still running</div>
          </div>
        </div>
        <div
          class="card bg-base-200"
          id="stat-success"
          title="completed / (completed + failed, lost or no answer)"
        >
          <div class="card-body p-4">
            <div class="text-sm opacity-60">Success rate</div>
            <div class="text-2xl font-semibold">{pct(@all.success_pct)}</div>
            <div class="text-xs opacity-60">{@all.failures} failed</div>
          </div>
        </div>
        <div
          class="card bg-base-200"
          id="stat-provision"
          title="CreateRunner sent until the runner is up"
        >
          <div class="card-body p-4">
            <div class="text-sm opacity-60">Time to runner, p95</div>
            <div class="text-2xl font-semibold">{Summary.fmt_ms(@all.provision.p95)}</div>
          </div>
        </div>
        <div
          class="card bg-base-200"
          id="stat-wait"
          title="queued on the forge until a runner took it"
        >
          <div class="card-body p-4">
            <div class="text-sm opacity-60">Queue wait, p50</div>
            <div class="text-2xl font-semibold">{Summary.fmt_ms(@jobs.wait.p50)}</div>
            <div :if={@jobs.wait.count == 0} class="text-xs opacity-60">
              needs GitHub or Forgejo webhooks
            </div>
          </div>
        </div>
      </div>

      <div
        :if={@runs != [] or @jobs.jobs > 0}
        id={"metrics-charts-#{@range}"}
        phx-hook="MetricsCharts"
        phx-update="ignore"
        data-charts={@charts}
        class="grid gap-4 lg:grid-cols-2"
      >
        <section :if={@runs != []} class="card bg-base-200 lg:col-span-2">
          <div class="card-body">
            <h2 class="card-title">Runner outcomes</h2>
            <p class="text-sm opacity-60">Attempts by when they were requested</p>
            <div class="relative h-64"><canvas data-chart="outcomes"></canvas></div>
          </div>
        </section>
        <section :if={@runs != []} class="card bg-base-200">
          <div class="card-body">
            <h2 class="card-title">Time to runner, p95</h2>
            <p class="text-sm opacity-60">CreateRunner sent until the runner is up, busiest agents</p>
            <div class="relative h-64"><canvas data-chart="provision"></canvas></div>
          </div>
        </section>
        <section :if={@runs != []} class="card bg-base-200">
          <div class="card-body">
            <h2 class="card-title">Steps to runner up, p50</h2>
            <p class="text-sm opacity-60">
              Reported by the agent, skipped steps count as 0. Older agents leave this blank
            </p>
            <div class="relative h-64"><canvas data-chart="phases"></canvas></div>
          </div>
        </section>
        <section :if={@jobs.wait.count > 0} class="card bg-base-200 lg:col-span-2">
          <div class="card-body">
            <h2 class="card-title">Queue wait</h2>
            <p class="text-sm opacity-60">Queued on the forge until a runner took it</p>
            <div class="relative h-64"><canvas data-chart="wait"></canvas></div>
          </div>
        </section>
      </div>

      <section :if={@runs != []} class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Agents</h2>
          <p class="text-sm opacity-60">
            p50, with p95 below. Step times come from the agent's clock
          </p>
          <.group_table id="agents" groups={@agents} steps={@steps} first="Agent" link />
        </div>
      </section>

      <section :if={@runs != []} class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Images</h2>
          <p class="text-sm opacity-60">
            Agents that don't report their image are grouped by the runner's labels
          </p>
          <.group_table id="images" groups={@images} steps={@steps} first="Image" />
        </div>
      </section>

      <section :if={@installs != []} class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Runner installs</h2>
          <p class="text-sm opacity-60">
            Where each image's runner came from and how long getting it onto the VM took
          </p>
          <table class="table table-sm" id="installs">
            <thead>
              <tr>
                <th>Image</th><th>Source</th><th>Runs</th><th>Install</th>
              </tr>
            </thead>
            <tbody>
              <tr :for={i <- @installs}>
                <td><code>{i.image}</code></td>
                <td title={elem(install_source_view(i.source), 1)}>
                  {elem(install_source_view(i.source), 0)}
                </td>
                <td>{i.runs}</td>
                <td class="whitespace-nowrap"><.stat s={i.install} /></td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <section :if={@jobs.jobs > 0} class="card bg-base-200" id="jobs">
        <div class="card-body">
          <h2 class="card-title">Jobs</h2>
          <p class="text-sm opacity-60">
            {@jobs.jobs} jobs, {if @jobs.mean_attempts,
              do: :erlang.float_to_binary(@jobs.mean_attempts, decimals: 2),
              else: "-"} runners each on average
          </p>
          <p :if={@jobs.wait.count == 0} class="text-sm opacity-60">
            Queue wait needs the forge to say when a job starts, which only webhook mode does
            (GitHub, or Forgejo 17+).
          </p>
          <table class="table table-sm">
            <thead>
              <tr>
                <th>Where the wait went</th><th>p50</th><th>p95</th>
              </tr>
            </thead>
            <tbody>
              <tr :for={
                {label, hint, s} <- [
                  {"Forge to coordinator", "webhook delivery or the next poll", @jobs.to_coordinator},
                  {"Waiting for an agent", "no capacity, and earlier failed runners", @jobs.dispatch},
                  {"Runner starting", "clone, boot, install and setup", @jobs.provision},
                  {"Runner taking the job", nil, @jobs.pickup}
                ]
              }>
                <td>
                  {label}
                  <div :if={hint} class="text-xs opacity-60">{hint}</div>
                </td>
                <td>{Summary.fmt_ms(s.p50)}</td>
                <td>{Summary.fmt_ms(s.p95)}</td>
              </tr>
              <tr class="font-semibold">
                <td>Total</td>
                <td>{Summary.fmt_ms(@jobs.wait.p50)}</td>
                <td>{Summary.fmt_ms(@jobs.wait.p95)}</td>
              </tr>
            </tbody>
          </table>
          <p class="text-xs opacity-60">
            {@jobs.ran} ran, {@jobs.pending} pending, {@jobs.abandoned} abandoned, {@jobs.cancelled} cancelled, {@jobs.gone} taken elsewhere or gone
          </p>
        </div>
      </section>
    </Layouts.admin>
    """
  end
end
