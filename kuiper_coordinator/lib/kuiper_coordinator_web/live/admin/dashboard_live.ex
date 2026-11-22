defmodule KuiperCoordinatorWeb.Admin.DashboardLive do
  @moduledoc """
  Admin dashboard (parity UI2, UI3): counts, capacity, pending jobs, agents with their
  availability switch (#40), and registration tokens. Register agent opens a modal that
  creates a token and shows its `kfr1_` bundle once, as a copyable command (D60); closing the
  modal drops the bundle. Pending tokens can be deleted. It refreshes on agent events and every few seconds for DB-side changes.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.{Admin, AgentRegistry, Agents}
  alias KuiperCoordinator.Admin.Dashboard

  @refresh_ms 5_000

  @impl true
  def mount(_params, _session, socket) do
    if connected?(socket) do
      Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, AgentRegistry.topic())
      :timer.send_interval(@refresh_ms, :refresh)
    end

    {:ok, socket |> assign(page_title: "Dashboard") |> close_register() |> load()}
  end

  defp load(socket), do: assign(socket, :dash, Dashboard.build())

  defp close_register(socket),
    do: assign(socket, register_open: false, new_bundle: nil, register_error: nil)

  @impl true
  def handle_info(:refresh, socket), do: {:noreply, load(socket)}
  def handle_info({:agent_status, _, _}, socket), do: {:noreply, socket}
  def handle_info({:agent_event, _, _}, socket), do: {:noreply, socket}
  def handle_info(_agent_change, socket), do: {:noreply, load(socket)}

  @impl true
  def handle_event("register_agent", _params, socket) do
    socket = assign(socket, register_open: true, new_bundle: nil, register_error: nil)

    case Agents.create_registration_token(3600, socket.assigns.current_admin.username) do
      {:ok, token} ->
        case bundle(token) do
          {:ok, bundle} ->
            {:noreply, socket |> assign(new_bundle: bundle) |> load()}

          {:error, reason} ->
            Agents.delete_registration_token(token.token)

            {:noreply,
             socket
             |> assign(register_error: "Couldn't build the registration bundle: #{reason}")
             |> load()}
        end

      {:error, _} ->
        {:noreply, assign(socket, register_error: "Failed to create token")}
    end
  end

  def handle_event("close_register", _params, socket), do: {:noreply, close_register(socket)}

  def handle_event("delete_token", %{"token" => token}, socket) do
    Agents.delete_registration_token(token)
    {:noreply, load(socket)}
  end

  def handle_event("set_available", %{"agent" => agent_id} = params, socket) do
    available? = params["available"] == "true"

    socket =
      case Agents.set_unavailable(agent_id, not available?) do
        :ok -> socket
        {:error, :not_found} -> put_flash(socket, :error, "Agent not found")
      end

    {:noreply, load(socket)}
  end

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:dashboard}>
      <.header>
        Dashboard
        <:actions>
          <.button id="register-agent" phx-click="register_agent">Register agent</.button>
        </:actions>
      </.header>

      <div class="stats stats-vertical lg:stats-horizontal shadow w-full" id="stats">
        <div class="stat">
          <div class="stat-title">Connected agents</div>
          <div class="stat-value" id="stat-connected">{@dash.connected_agents}</div>
        </div>
        <div class="stat">
          <div class="stat-title">Registered agents</div>
          <div class="stat-value" id="stat-registered">{length(@dash.agents)}</div>
        </div>
        <div class="stat" title="VMs in use out of the most the connected agents run">
          <div class="stat-title">Capacity</div>
          <div class="stat-value" id="stat-capacity">
            {@dash.capacity.used}<span class="text-base opacity-60"> / {@dash.capacity.total}</span>
          </div>
          <div :if={@dash.capacity.external > 0} class="stat-desc">
            {@dash.capacity.external} external
          </div>
          <div :for={l <- @dash.capacity.limits} class="stat-desc">
            {Dashboard.limit_label(l.name)} {l.used} / {l.max}<span :if={l.external > 0}>, {l.external} external</span>
          </div>
        </div>
        <div class="stat">
          <div class="stat-title">Active runners</div>
          <div class="stat-value" id="stat-runners">{@dash.active_runners}</div>
        </div>
        <div class="stat">
          <div class="stat-title">Pending jobs</div>
          <div class="stat-value" id="stat-pending">{length(@dash.pending_jobs)}</div>
        </div>
      </div>

      <section id="registration" class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Register an agent</h2>
          <p class="text-sm opacity-70">Each token registers one agent and expires after an hour.</p>
          <p :if={@dash.tokens == []} class="text-sm opacity-60">No pending tokens.</p>
          <table :if={@dash.tokens != []} class="table table-sm" id="tokens">
            <thead>
              <tr>
                <th>Pending token</th><th>Created by</th><th>Expires</th><th></th>
              </tr>
            </thead>
            <tbody>
              <tr :for={t <- @dash.tokens} id={"token-#{t.token}"}>
                <td><code>{t.token}</code></td>
                <td>{t.created_by}</td>
                <td>{Calendar.strftime(t.expires_at, "%Y-%m-%d %H:%M UTC")}</td>
                <td>
                  <button
                    class="btn btn-error btn-xs"
                    phx-click="delete_token"
                    phx-value-token={t.token}
                    data-confirm="Delete this token?"
                  >
                    Delete
                  </button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <section class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Pending jobs</h2>
          <p :if={@dash.pending_jobs == []} class="opacity-60">No jobs waiting.</p>
          <table :if={@dash.pending_jobs != []} class="table table-sm" id="pending-jobs">
            <thead>
              <tr>
                <th>Job</th><th>Repository</th><th>Labels</th><th>Waiting</th><th>Status</th><th>
                  Retries
                </th>
              </tr>
            </thead>
            <tbody>
              <tr :for={j <- @dash.pending_jobs} id={"job-#{j.job_key}"}>
                <td>
                  <span :if={j.workflow_name}>{j.workflow_name} / </span>{j.job_name || "job"}
                  <div class="text-xs opacity-60">{j.job_key}</div>
                </td>
                <td>{j.repository || "-"}</td>
                <td><span :for={l <- j.labels} class="badge badge-ghost badge-sm mr-1">{l}</span></td>
                <td>{j.waiting}</td>
                <td>{job_status(assigns |> Map.put(:j, j))}</td>
                <td>
                  {j.retry_count}
                  <div :if={j.failed_agents > 0} class="text-xs opacity-60">
                    {j.failed_agents} agent(s) failed
                  </div>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <section class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Agents</h2>
          <p :if={@dash.agents == []} class="opacity-60">No agents registered yet.</p>
          <table :if={@dash.agents != []} class="table table-sm" id="agents">
            <thead>
              <tr>
                <th>Hostname / agent ID</th><th>Type</th><th>Version</th><th>Status</th><th>
                  Capacity
                </th><th>Labels</th><th>Available</th>
              </tr>
            </thead>
            <tbody>
              <tr :for={a <- @dash.agents} id={"agent-#{a.agent_id}"}>
                <td>
                  <.link navigate={~p"/admin/agents/#{a.agent_id}"} class="link">{a.hostname}</.link>
                  <div class="text-xs opacity-60">{a.agent_id}</div>
                </td>
                <td>{a.agent_type}</td>
                <td>
                  {a.version}<span
                    :if={!a.version}
                    class="opacity-60"
                    title="agent doesn't report its version"
                  >unknown</span>
                  <div :if={a.update}><.update_badge agent={a} /></div>
                </td>
                <td>
                  <span :if={a.revoked?} class="badge badge-neutral">Revoked</span>
                  <span :if={!a.revoked? and a.online?} class="badge badge-success badge-outline">Online</span>
                  <span :if={!a.revoked? and !a.online?} class="badge badge-error badge-outline">Offline</span>
                  <span
                    :if={!a.revoked? and a.unavailable_at}
                    class="badge badge-warning badge-outline"
                    title="no new runners, running ones finish"
                  >
                    Unavailable
                  </span>
                </td>
                <td>
                  {Dashboard.capacity_used(a)}/{Dashboard.capacity_max(a)}
                  <span :if={Dashboard.capacity_external(a) > 0} class="opacity-60">
                    ({Dashboard.capacity_external(a)} external)
                  </span>
                  <div
                    :for={l <- Enum.reject(a.limits, &(&1.name == "total"))}
                    class="text-xs opacity-60"
                  >
                    {Dashboard.limit_label(l.name)} {l.used}/{l.max}<span :if={l.external > 0}> ({l.external} external)</span>
                  </div>
                </td>
                <td>
                  <span :for={l <- a.base_labels} class="badge badge-ghost badge-sm mr-1">{l}</span>
                  <div :for={m <- a.mapping_labels} class="text-xs">
                    + <span :for={l <- m} class="badge badge-outline badge-sm mr-1">{l}</span>
                  </div>
                </td>
                <td>
                  <input
                    :if={!a.revoked?}
                    type="checkbox"
                    class="toggle toggle-sm toggle-success"
                    id={"available-#{a.agent_id}"}
                    checked={is_nil(a.unavailable_at)}
                    phx-click="set_available"
                    phx-value-agent={a.agent_id}
                    phx-value-available={to_string(not is_nil(a.unavailable_at))}
                    title="Off: no new runners, running ones finish"
                  />
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <.register_modal
        :if={@register_open}
        bundle={@new_bundle}
        error={@register_error}
      />
    </Layouts.admin>
    """
  end

  attr :bundle, :string, default: nil
  attr :error, :string, default: nil

  # DaisyUI modal (D51, D60): the bundle wraps inside the box instead of a flex column
  defp register_modal(assigns) do
    ~H"""
    <div
      id="register-modal"
      class="modal modal-open"
      role="dialog"
      aria-modal="true"
      aria-labelledby="register-title"
      phx-window-keydown="close_register"
      phx-key="Escape"
    >
      <div class="modal-box w-11/12 max-w-3xl">
        <h3 id="register-title" class="text-lg font-bold">Register an agent</h3>
        <div :if={@error} id="register-error" class="alert alert-error mt-4" role="alert">
          <.icon name="hero-exclamation-circle" class="size-5 shrink-0" />
          <span>{@error}</span>
        </div>
        <div :if={@bundle} class="mt-4 space-y-3">
          <p class="text-sm">
            Registration token created. Copy it now; it won't be shown again.
            It registers one agent and expires in an hour.
          </p>
          <p class="text-sm font-semibold">Run on the agent:</p>
          <pre
            id="register-command"
            class="bg-base-200 rounded-box p-3 text-xs font-mono whitespace-pre-wrap break-all select-all"
          >kuiper-tart-agent register '{@bundle}'</pre>
          <p class="text-xs opacity-70">
            Or use <code>kuiper-proxmox-agent</code>
            or <code>kuiper-libvirt-agent</code>
            in place of <code>kuiper-tart-agent</code>.
          </p>
        </div>
        <div class="modal-action">
          <button
            :if={@bundle}
            id="register-copy"
            type="button"
            class="btn btn-primary"
            phx-hook=".CopyText"
            data-copy-target="#register-command"
          >
            <.icon name="hero-clipboard-document" class="size-4" /> Copy
          </button>
          <button id="register-close" type="button" class="btn" phx-click="close_register">
            Close
          </button>
        </div>
      </div>
      <div class="modal-backdrop" phx-click="close_register"></div>
    </div>
    <script :type={Phoenix.LiveView.ColocatedHook} name=".CopyText">
      export default {
        mounted() {
          this.el.addEventListener("click", () => {
            const target = document.querySelector(this.el.dataset.copyTarget)
            if (!target) return
            const label = this.el.innerHTML
            navigator.clipboard.writeText(target.innerText.trim()).then(() => {
              this.el.textContent = "Copied"
              setTimeout(() => { this.el.innerHTML = label }, 1500)
            })
          })
        }
      }
    </script>
    """
  end

  # the bundle carries the CA, which is only loaded when agent gRPC is on
  defp bundle(token) do
    {:ok, Agents.registration_bundle(token, Admin.coordinator_url())}
  rescue
    # an unreadable or unloaded CA is a message on the page, never a crashed LiveView (D58)
    e ->
      require Logger
      Logger.error("Registration bundle failed: #{Exception.message(e)}")
      {:error, Exception.message(e)}
  end

  defp job_status(%{j: %{assigned_agent: agent}} = assigns) when is_binary(agent) do
    ~H"""
    <span class="badge badge-success badge-outline">Runner starting</span>
    <div class="text-xs opacity-60">on {@j.assigned_agent}</div>
    """
  end

  defp job_status(%{j: %{matching_agents: 0}} = assigns),
    do: ~H|<span class="badge badge-error">No matching agent</span>|

  defp job_status(%{j: %{matching_agents: n, unavailable_agents: n}} = assigns) do
    ~H"""
    <span class="badge badge-error badge-outline">Agents unavailable</span>
    <div class="text-xs opacity-60">{@j.matching_agents} matching, all marked unavailable</div>
    """
  end

  defp job_status(%{j: %{free_capacity: 0}} = assigns) do
    ~H"""
    <span class="badge badge-warning badge-outline">Agents busy</span>
    <div class="text-xs opacity-60">
      {@j.matching_agents} matching, all full<span :if={@j.unavailable_agents > 0}> or unavailable</span>
    </div>
    """
  end

  defp job_status(assigns) do
    ~H"""
    <span class="badge badge-outline">Waiting</span>
    <div class="text-xs opacity-60">{@j.matching_agents} matching, {@j.free_capacity} free</div>
    """
  end

  attr :agent, :map, required: true

  @doc "Links to the newer release when the agent is behind it (#75); shared with the agent page."
  def update_badge(assigns) do
    ~H"""
    <a
      class="badge badge-warning badge-outline badge-sm update-badge"
      href={@agent.update.url}
      target="_blank"
      rel="noopener"
      title={"kuiper-#{@agent.agent_type}-agent #{@agent.update.latest} is out#{released(@agent.update.published_at)}; this agent runs #{@agent.version}"}
    >
      {@agent.update.latest} available
    </a>
    """
  end

  defp released(nil), do: ""
  defp released(at), do: " (released #{Calendar.strftime(at, "%Y-%m-%d")})"
end
