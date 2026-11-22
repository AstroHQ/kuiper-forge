defmodule KuiperCoordinatorWeb.Admin.AgentLive do
  @moduledoc """
  Agent detail (parity UI4): registration, live limits, label split, this agent's runners,
  revoke, and the availability switch (#40: DB first, then scheduling via PubSub; running
  runners finish), its last 25 failures (#33) and its last 7 days of runs (#67, the
  metrics page's numbers). Unknown agents are a 404.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.{AgentFailures, AgentRegistry, Agents, Runners}
  alias KuiperCoordinator.Admin.Dashboard
  alias KuiperCoordinatorWeb.Admin.NotFound

  @refresh_ms 5_000

  @impl true
  def mount(%{"agent_id" => agent_id}, _session, socket) do
    if connected?(socket) do
      Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, AgentRegistry.topic())
      :timer.send_interval(@refresh_ms, :refresh)
    end

    {:ok, socket |> assign(agent_id: agent_id) |> load()}
  end

  defp load(socket) do
    agent = Agents.get_agent(socket.assigns.agent_id) || raise NotFound
    connected = Map.new(AgentRegistry.list(), &{&1.agent_id, &1})
    runners = Enum.filter(Runners.list_runners(), &(&1.agent_id == agent.agent_id))
    now = DateTime.utc_now()

    failures =
      for f <- AgentFailures.recent(agent.agent_id, 25),
          do: Map.put(f, :ago, Dashboard.format_age(DateTime.diff(now, f.occurred_at)))

    assign(socket,
      page_title: agent.hostname,
      agent: Dashboard.agent_summary(agent, connected, runners),
      runners: runners,
      failures: failures,
      performance: KuiperCoordinatorWeb.Admin.MetricsLive.agent_performance(agent.agent_id)
    )
  end

  @impl true
  def handle_info(:refresh, socket), do: {:noreply, load(socket)}
  def handle_info({:agent_status, _, _}, socket), do: {:noreply, socket}
  def handle_info({:agent_event, _, _}, socket), do: {:noreply, socket}
  def handle_info(_agent_change, socket), do: {:noreply, load(socket)}

  @impl true
  def handle_event("set_available", %{"available" => available}, socket) do
    socket =
      case Agents.set_unavailable(socket.assigns.agent_id, available != "true") do
        :ok -> socket
        {:error, :not_found} -> put_flash(socket, :error, "Agent not found")
      end

    {:noreply, load(socket)}
  end

  def handle_event("revoke", _params, socket) do
    case Agents.revoke_agent(socket.assigns.agent_id) do
      :ok ->
        {:noreply,
         socket
         |> put_flash(:info, "Agent #{socket.assigns.agent.hostname} revoked")
         |> push_navigate(to: ~p"/admin/dashboard")}

      {:error, :not_found} ->
        {:noreply, socket |> put_flash(:error, "Agent not found") |> load()}
    end
  end

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:dashboard}>
      <.header>
        {@agent.hostname}
        <:subtitle>{@agent.agent_id}</:subtitle>
        <:actions>
          <.link navigate={~p"/admin/agents/#{@agent.agent_id}/logs"} class="btn btn-sm">Logs</.link>
          <button
            :if={!@agent.revoked?}
            id="revoke"
            class="btn btn-error btn-sm"
            phx-click="revoke"
            data-confirm="Revoke this agent? Its certificate stops working and it disconnects now."
          >
            Revoke
          </button>
        </:actions>
      </.header>

      <section class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Agent</h2>
          <dl class="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1 text-sm">
            <dt class="opacity-60">Status</dt>
            <dd id="agent-status" class="flex gap-1 items-center">
              <span :if={@agent.revoked?} class="badge badge-neutral">Revoked</span>
              <span
                :if={!@agent.revoked? and @agent.online?}
                class="badge badge-success badge-outline"
              >Online</span>
              <span :if={!@agent.revoked? and !@agent.online?} class="badge badge-error badge-outline">Offline</span>
              <span
                :if={!@agent.revoked? and @agent.unavailable_at}
                class="badge badge-warning badge-outline"
                title={"since #{Calendar.strftime(@agent.unavailable_at, "%Y-%m-%d %H:%M UTC")}"}
              >
                Unavailable
              </span>
            </dd>
            <dt class="opacity-60">Type</dt>
            <dd>{@agent.agent_type}</dd>
            <dt class="opacity-60">Version</dt>
            <dd id="agent-version">
              {@agent.version || "unknown"}
              <KuiperCoordinatorWeb.Admin.DashboardLive.update_badge
                :if={@agent.update}
                agent={@agent}
              />
            </dd>
            <dt class="opacity-60">Registered</dt>
            <dd>{Calendar.strftime(@agent.created_at, "%Y-%m-%d %H:%M UTC")}</dd>
            <dt class="opacity-60">Capacity</dt>
            <dd id="agent-capacity">
              {Dashboard.capacity_used(@agent)}/{Dashboard.capacity_max(@agent)}
              <span :if={Dashboard.capacity_external(@agent) > 0} class="opacity-60">
                ({Dashboard.capacity_external(@agent)} external)
              </span>
            </dd>
            <dt class="opacity-60">Limits</dt>
            <dd id="agent-limits">
              <span :if={@agent.limits == []} class="opacity-60">reported when connected</span>
              <div :for={l <- @agent.limits}>
                {Dashboard.limit_label(l.name)} {l.used}/{l.max}<span
                  :if={l.external > 0}
                  class="opacity-60"
                > ({l.external} external)</span>
              </div>
            </dd>
            <dt class="opacity-60">Labels</dt>
            <dd id="agent-labels">
              <span :for={l <- @agent.base_labels} class="badge badge-ghost badge-sm mr-1">{l}</span>
              <div :for={m <- @agent.mapping_labels} class="text-xs">
                + <span :for={l <- m} class="badge badge-outline badge-sm mr-1">{l}</span>
              </div>
            </dd>
            <dt :if={!@agent.revoked?} class="opacity-60">Available</dt>
            <dd :if={!@agent.revoked?}>
              <input
                type="checkbox"
                class="toggle toggle-sm toggle-success"
                id={"available-#{@agent.agent_id}"}
                checked={is_nil(@agent.unavailable_at)}
                phx-click="set_available"
                phx-value-available={to_string(not is_nil(@agent.unavailable_at))}
                title="Off: no new runners, running ones finish"
              />
            </dd>
          </dl>
        </div>
      </section>

      <KuiperCoordinatorWeb.Admin.MetricsLive.performance_card :if={@performance} g={@performance} />

      <section class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Runners</h2>
          <p :if={@runners == []} class="opacity-60">No runners on this agent.</p>
          <table :if={@runners != []} class="table table-sm" id="runners">
            <thead>
              <tr>
                <th>Runner</th><th>Job</th><th>Repository</th><th>Created</th>
              </tr>
            </thead>
            <tbody>
              <tr :for={r <- @runners} id={"runner-#{r.runner_name}"}>
                <td>
                  {r.runner_name}
                  <div :if={r.vm_name != r.runner_name} class="text-xs opacity-60">{r.vm_name}</div>
                </td>
                <td>
                  <span :if={r.workflow_name}>{r.workflow_name} / </span>{r.job_name ||
                    (r.pool && "pool #{r.pool}") || "-"}
                </td>
                <td>{r.repository || "-"}</td>
                <td>{Calendar.strftime(r.inserted_at, "%Y-%m-%d %H:%M UTC")}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <section class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">
            Recent failures
            <.link
              navigate={~p"/admin/errors?agent=#{@agent.agent_id}"}
              class="link text-sm font-normal"
            >
              all
            </.link>
          </h2>
          <p :if={@failures == []} class="opacity-60">No failures recorded.</p>
          <KuiperCoordinatorWeb.Admin.ErrorsLive.failures
            :if={@failures != []}
            id="agent-failures"
            failures={@failures}
          />
        </div>
      </section>
    </Layouts.admin>
    """
  end
end
