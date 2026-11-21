defmodule KuiperCoordinatorWeb.Admin.AgentLogsLive do
  @moduledoc """
  An agent's uploaded logs (parity UI5, agent_log_routes.rs): level filter, `before` cursor
  paging 500 at a time, shown oldest first, live tail on the newest page only, 404 for
  unregistered agents.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.Agents
  alias KuiperCoordinatorWeb.Admin.NotFound

  @page_size 500
  @tail_ms 2_000
  @levels ~w(error warn info debug trace)

  @impl true
  def mount(%{"agent_id" => agent_id}, _session, socket) do
    agent = Agents.get_agent(agent_id) || raise NotFound
    {:ok, assign(socket, agent: agent, page_title: "#{agent.hostname} logs", timer: nil)}
  end

  @impl true
  def handle_params(params, _uri, socket) do
    min_level = Agents.level_filter(params["level"])
    before = presence(params["before"])
    live? = params["live"] not in [nil, ""] and is_nil(before)

    socket =
      socket
      |> assign(
        level: Agents.level_name(min_level),
        min_level: min_level,
        before: before,
        live?: live?
      )
      |> load()
      |> tail(live?)

    {:noreply, socket}
  end

  defp presence(""), do: nil
  defp presence(v), do: v

  defp load(socket) do
    %{agent: agent, min_level: min_level, before: before} = socket.assigns
    lines = Agents.recent_logs(agent.agent_id, min_level, before, @page_size)
    older = if length(lines) == @page_size, do: Agents.log_cursor(List.last(lines))
    assign(socket, lines: Enum.reverse(lines), older: older)
  end

  defp tail(socket, live?) do
    if t = socket.assigns.timer, do: :timer.cancel(t)

    timer =
      if live? and connected?(socket) do
        {:ok, t} = :timer.send_interval(@tail_ms, :tail)
        t
      end

    assign(socket, timer: timer)
  end

  @impl true
  def handle_info(:tail, socket), do: {:noreply, load(socket)}

  @impl true
  def handle_event("filter", params, socket) do
    {:noreply,
     push_patch(socket,
       to: path(socket.assigns.agent.agent_id, params["level"], nil, params["live"] == "true")
     )}
  end

  defp path(agent_id, level, before, live?) do
    query =
      [level: level, before: before, live: live? && "1"]
      |> Enum.reject(fn {_, v} -> v in [nil, false, "", "trace"] end)

    ~p"/admin/agents/#{agent_id}/logs?#{query}"
  end

  @impl true
  def render(assigns) do
    assigns = assign(assigns, levels: @levels)

    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:dashboard}>
      <.header>
        {@agent.hostname} logs
        <:subtitle>
          <.link navigate={~p"/admin/agents/#{@agent.agent_id}"} class="link">{@agent.agent_id}</.link>
        </:subtitle>
      </.header>

      <form id="log-filter" phx-change="filter" class="flex gap-4 items-center">
        <select id="level" name="level" class="select select-sm">
          <option :for={l <- @levels} value={l} selected={l == @level}>{l}</option>
        </select>
        <label :if={is_nil(@before)} class="label gap-2" id="live">
          <input type="hidden" name="live" value="false" />
          <input type="checkbox" name="live" value="true" class="toggle toggle-sm" checked={@live?} />
          Live
        </label>
        <.link
          :if={@before}
          patch={path(@agent.agent_id, @level, nil, false)}
          class="link text-sm"
          id="newest-logs"
        >
          Newest
        </.link>
      </form>

      <p :if={@lines == []} class="opacity-60">
        No logs from this agent yet. Older agent versions don't upload logs, and newer ones start after they connect.
      </p>
      <.link
        :if={@older}
        patch={path(@agent.agent_id, @level, @older, false)}
        class="link text-sm"
        id="older-logs"
      >
        Older lines
      </.link>
      <div
        :if={@lines != []}
        id="log-lines"
        class="font-mono text-xs bg-base-200 rounded p-2 overflow-x-auto"
      >
        <div :for={l <- @lines} class={["whitespace-pre-wrap", level_class(l.level)]}>
          <span class="opacity-60">{format_ts(l.ts)}</span>
          <span class="font-semibold">{String.upcase(Agents.level_name(l.level))}</span>
          <span class="opacity-60">{l.target}</span>
          {l.message}
        </div>
      </div>
    </Layouts.admin>
    """
  end

  defp level_class(1), do: "text-error"
  defp level_class(2), do: "text-warning"
  defp level_class(_), do: nil

  defp format_ts(micros) do
    case DateTime.from_unix(micros, :microsecond) do
      {:ok, dt} -> Calendar.strftime(dt, "%Y-%m-%d %H:%M:%S.%f")
      _ -> to_string(micros)
    end
  end
end
