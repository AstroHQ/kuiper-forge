defmodule KuiperCoordinatorWeb.Admin.ErrorsLive do
  @moduledoc """
  Recent errors across agents (parity UI7, #74, error_routes.rs): kind and agent filters (an
  unknown kind shows everything), the newest 200, last-24-hour counts per agent by kind
  (unknown kinds count toward the total, 33b1c0f), hostname falling back to the agent id.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.{AgentFailures, Agents}
  alias KuiperCoordinator.Admin.Dashboard

  @limit 200

  @impl true
  def mount(_params, _session, socket),
    do: {:ok, assign(socket, page_title: "Errors", limit: @limit)}

  @impl true
  def handle_params(params, _uri, socket) do
    kind = AgentFailures.parse_kind(params["kind"])
    agent = if is_binary(params["agent"]) and params["agent"] != "", do: params["agent"]
    now = DateTime.utc_now()

    agents =
      Agents.list_agents() |> Enum.map(&{&1.agent_id, &1.hostname}) |> Enum.sort_by(&elem(&1, 1))

    names = Map.new(agents)
    hostname = &Map.get(names, &1, &1)

    last_day =
      AgentFailures.count_rows(AgentFailures.counts_since(DateTime.add(now, -86_400)), hostname)

    totals =
      last_day
      |> Enum.map(& &1.counts)
      |> Enum.zip_with(&Enum.sum/1)
      |> then(&if(&1 == [], do: List.duplicate(0, length(AgentFailures.kinds())), else: &1))

    failures =
      for f <- AgentFailures.recent_all(kind, agent, @limit) do
        Map.merge(f, %{
          hostname: hostname.(f.agent_id),
          ago: Dashboard.format_age(DateTime.diff(now, f.occurred_at))
        })
      end

    {:noreply,
     assign(socket,
       kind: kind,
       agent: agent,
       agents: agents,
       last_day: last_day,
       totals: totals,
       failures: failures
     )}
  end

  @impl true
  def handle_event("filter", params, socket) do
    query =
      Enum.reject([kind: params["kind"], agent: params["agent"]], fn {_, v} -> v in [nil, ""] end)

    {:noreply, push_patch(socket, to: ~p"/admin/errors?#{query}")}
  end

  @impl true
  def render(assigns) do
    assigns = assign(assigns, kinds: AgentFailures.kinds())

    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:errors}>
      <.header>
        Errors
        <:subtitle>Runner and command failures agents reported, newest first.</:subtitle>
      </.header>

      <section class="card bg-base-200">
        <div class="card-body">
          <h2 class="card-title">Last 24 hours</h2>
          <p :if={@last_day == []} class="opacity-60">No failures in the last 24 hours.</p>
          <table :if={@last_day != []} class="table table-sm" id="last-day">
            <thead>
              <tr>
                <th>Agent</th><th :for={{_, label} <- @kinds}>{label}</th><th>Total</th>
              </tr>
            </thead>
            <tbody>
              <tr :for={r <- @last_day} id={"count-#{r.agent_id}"}>
                <td>
                  <.link navigate={~p"/admin/agents/#{r.agent_id}"} class="link">{r.hostname}</.link>
                </td>
                <td :for={n <- r.counts}>{n}</td>
                <td class="font-semibold">{r.total}</td>
              </tr>
              <tr id="count-totals" class="font-semibold">
                <td>All agents</td><td :for={n <- @totals}>{n}</td><td>
                  {Enum.sum(Enum.map(@last_day, & &1.total))}
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <form id="error-filter" phx-change="filter" class="flex gap-4">
        <select id="kind" name="kind" class="select select-sm">
          <option value="">All kinds</option>
          <option :for={{k, label} <- @kinds} value={k} selected={k == @kind}>{label}</option>
        </select>
        <select id="agent" name="agent" class="select select-sm">
          <option value="">All agents</option>
          <option :for={{id, host} <- @agents} value={id} selected={id == @agent}>{host}</option>
        </select>
      </form>

      <p :if={@failures == []} class="opacity-60">No failures match.</p>
      <.failures :if={@failures != []} id="failures" failures={@failures} show_agent />
      <p :if={length(@failures) == @limit} class="text-sm opacity-60">Showing the newest {@limit}.</p>
    </Layouts.admin>
    """
  end

  attr :id, :string, required: true
  attr :failures, :list, required: true
  attr :show_agent, :boolean, default: false

  @doc "A failures table, shared with the agent page."
  def failures(assigns) do
    ~H"""
    <table class="table table-sm" id={@id}>
      <thead>
        <tr>
          <th>When</th><th :if={@show_agent}>Agent</th><th>Kind</th><th>Runner / job</th><th>
            Message
          </th>
        </tr>
      </thead>
      <tbody>
        <tr :for={f <- @failures} id={"failure-#{f.id}"}>
          <td title={Calendar.strftime(f.occurred_at, "%Y-%m-%d %H:%M:%S UTC")}>{f.ago} ago</td>
          <td :if={@show_agent}>
            <.link navigate={~p"/admin/agents/#{f.agent_id}"} class="link">{f.hostname}</.link>
          </td>
          <td>
            <span class="badge badge-error badge-outline badge-sm">{AgentFailures.label(f.kind)}</span>
          </td>
          <td>
            {f.runner_name || "-"}
            <div :if={f.job_key} class="text-xs opacity-60">{f.job_key}</div>
          </td>
          <td class="whitespace-pre-wrap break-all">{f.message}</td>
        </tr>
      </tbody>
    </table>
    """
  end
end
