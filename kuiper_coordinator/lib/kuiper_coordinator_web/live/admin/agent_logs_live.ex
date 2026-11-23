defmodule KuiperCoordinatorWeb.Admin.AgentLogsLive do
  @moduledoc """
  An agent's uploaded logs (parity UI5, agent_log_routes.rs + agent_logs.html): level filter,
  `before` cursor paging 500 at a time, shown oldest first in a bounded dark terminal box, every
  level colored, a fixed-width level column, and times in the browser's local zone.

  Live tail on the newest page only: uploads are announced on PubSub (`Agents.subscribe_logs/1`)
  and only the new lines are appended to the stream (the newest 500 stay in the DOM). The `.LogView`
  hook follows the bottom, pauses when you scroll up, and shows a "Jump to newest" button.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.Agents
  alias KuiperCoordinatorWeb.Admin.NotFound

  @page_size 500
  @levels [
    {"trace", "All"},
    {"debug", "Debug and up"},
    {"info", "Info and up"},
    {"warn", "Warnings and errors"},
    {"error", "Errors only"}
  ]

  @impl true
  def mount(%{"agent_id" => agent_id}, _session, socket) do
    agent = Agents.get_agent(agent_id) || raise NotFound

    {:ok,
     socket
     |> assign(agent: agent, page_title: "#{agent.hostname} logs", subscribed?: false)
     |> stream_configure(:lines, dom_id: &"log-#{&1.id}")}
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
      |> follow(live?)

    {:noreply, socket}
  end

  defp presence(v) when is_binary(v) and v != "", do: v
  defp presence(_), do: nil

  defp load(socket) do
    %{agent: agent, min_level: min_level, before: before} = socket.assigns
    # taken before the page, so a line stored in between is appended, never lost
    last_id = Agents.last_log_id(agent.agent_id)
    lines = Agents.recent_logs(agent.agent_id, min_level, before, @page_size)
    older = if length(lines) == @page_size, do: Agents.log_cursor(List.last(lines))

    socket
    |> assign(older: older, empty?: lines == [], last_id: last_id)
    |> stream(:lines, Enum.reverse(lines), reset: true)
  end

  defp follow(socket, live?) do
    %{agent: agent, subscribed?: subscribed?} = socket.assigns

    cond do
      live? and not subscribed? and connected?(socket) ->
        Agents.subscribe_logs(agent.agent_id)
        assign(socket, subscribed?: true)

      not live? and subscribed? ->
        Agents.unsubscribe_logs(agent.agent_id)
        assign(socket, subscribed?: false)

      true ->
        socket
    end
  end

  @impl true
  def handle_info(msg, socket) when msg in [:tail, :new_logs] do
    %{agent: agent, min_level: min_level, last_id: last_id, live?: live?} = socket.assigns

    if live? do
      lines = Agents.logs_after(agent.agent_id, min_level, last_id, @page_size)
      last_id = Enum.reduce(lines, last_id, &max(&1.id, &2))

      socket =
        Enum.reduce(lines, socket, &stream_insert(&2, :lines, &1, limit: -@page_size))

      {:noreply, assign(socket, last_id: last_id, empty?: socket.assigns.empty? and lines == [])}
    else
      {:noreply, socket}
    end
  end

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

      <form id="log-filter" phx-change="filter" class="flex flex-wrap gap-4 items-center mb-3">
        <label class="label gap-2 font-medium text-base-content" for="level">
          Level
          <select id="level" name="level" class="select select-sm w-auto">
            <option :for={{l, label} <- @levels} value={l} selected={l == @level}>{label}</option>
          </select>
        </label>
        <label :if={is_nil(@before)} class="label gap-2 font-medium text-base-content" id="live">
          <input type="hidden" name="live" value="false" />
          <input type="checkbox" name="live" value="true" class="toggle toggle-sm" checked={@live?} />
          Live
        </label>
        <.link
          :if={@older}
          patch={path(@agent.agent_id, @level, @older, false)}
          class="btn btn-sm btn-outline"
          id="older-logs"
        >
          Older
        </.link>
        <.link
          :if={@before}
          patch={path(@agent.agent_id, @level, nil, false)}
          class="btn btn-sm btn-outline"
          id="newest-logs"
        >
          Newest
        </.link>
        <span class="text-sm opacity-60">
          times are <span class="local-tz" id="log-tz" phx-update="ignore">UTC</span>, from the agent's clock
        </span>
      </form>

      <p :if={@empty? and (@before || @level != "trace")} class="opacity-60" id="log-empty">
        No log lines match.
      </p>
      <p :if={@empty? and is_nil(@before) and @level == "trace"} class="opacity-60" id="log-empty">
        No logs from this agent yet. Older agent versions don't upload logs, and newer ones start after they connect.
      </p>

      <div class={["relative", @empty? && "hidden"]}>
        <div
          id="log-view"
          phx-hook=".LogView"
          phx-update="stream"
          data-follow={to_string(is_nil(@before))}
          class="log-view bg-neutral-950 text-neutral-300 border border-base-300 rounded-box px-4 py-3 font-mono text-xs leading-relaxed max-h-[70vh] overflow-auto"
        >
          <div :for={{id, l} <- @streams.lines} id={id} class="log-line flex gap-x-2">
            <time
              class="log-time shrink-0 text-neutral-500"
              datetime={iso(l.ts)}
              data-us={l.ts}
              title={utc(l.ts, "%Y-%m-%d %H:%M:%S.%f") <> " UTC"}
            >{utc(l.ts, "%Y-%m-%d %H:%M:%S.%3f")}</time>
            <span class={["log-level shrink-0 whitespace-pre", level_class(l.level)]}>{label(l)}</span>
            <span class="min-w-0 whitespace-pre-wrap break-words"><span class="log-target text-neutral-400">{l.target}</span> {l.message}</span>
          </div>
        </div>
        <div id="log-jump-wrap" phx-update="ignore">
          <button
            id="log-jump"
            type="button"
            class="btn btn-sm btn-primary absolute bottom-3 right-6 shadow hidden"
          >
            <.icon name="hero-arrow-down" class="size-4" /> Jump to newest
          </button>
        </div>
      </div>
    </Layouts.admin>
    <script :type={Phoenix.LiveView.ColocatedHook} name=".LogView">
      // Follow-tail like a terminal: stick to the bottom while new lines stream in, stop when the
      // reader scrolls up (the jump button brings it back), and show agent times in local time.
      const pad = (n, w = 2) => String(n).padStart(w, "0")
      const localTime = (el) => {
        const us = Number(el.dataset.us)
        if (!Number.isFinite(us) || el.dataset.local) return
        const d = new Date(Math.floor(us / 1000))
        el.textContent = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
          `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`
        el.title = `${d.toString()}\n${el.title}`
        el.dataset.local = "1"
      }
      export default {
        mounted() {
          this.jump = document.getElementById("log-jump")
          this.following = this.el.dataset.follow === "true"
          const zone = Intl.DateTimeFormat().resolvedOptions().timeZone
          if (zone) for (const z of document.querySelectorAll(".local-tz")) z.textContent = zone
          this.localize()
          if (this.following) this.toBottom()
          this.onScroll = () => {
            const atBottom = this.el.scrollHeight - this.el.scrollTop - this.el.clientHeight < 8
            if (this.el.dataset.follow === "true") this.following = atBottom
            this.jump.classList.toggle("hidden", atBottom)
          }
          this.el.addEventListener("scroll", this.onScroll, {passive: true})
          this.jump.addEventListener("click", () => { this.following = true; this.toBottom() })
          // stream inserts don't always re-run updated() on the container, so watch the children
          this.observer = new MutationObserver(() => {
            this.localize()
            if (this.following) this.toBottom()
            else this.onScroll()
          })
          this.observer.observe(this.el, {childList: true})
        },
        updated() { this.localize() },
        destroyed() { this.observer && this.observer.disconnect() },
        localize() { for (const t of this.el.querySelectorAll("time[data-us]")) localTime(t) },
        toBottom() { this.el.scrollTop = this.el.scrollHeight; this.jump.classList.add("hidden") }
      }
    </script>
    """
  end

  # the box is dark in every theme, so fixed colors rather than the theme's
  defp level_class(1), do: "log-error text-red-400 font-bold"
  defp level_class(2), do: "log-warn text-amber-300"
  defp level_class(3), do: "log-info text-emerald-400"
  defp level_class(4), do: "log-debug text-sky-400"
  defp level_class(_), do: "log-trace text-violet-400"

  defp label(%{level: level}),
    do: level |> Agents.level_name() |> String.upcase() |> String.pad_leading(5)

  defp utc(micros, fmt) do
    case DateTime.from_unix(micros, :microsecond) do
      {:ok, dt} -> Calendar.strftime(dt, fmt)
      _ -> to_string(micros)
    end
  end

  defp iso(micros) do
    case DateTime.from_unix(micros, :microsecond) do
      {:ok, dt} -> DateTime.to_iso8601(dt)
      _ -> nil
    end
  end
end
