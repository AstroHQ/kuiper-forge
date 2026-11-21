defmodule KuiperCoordinatorWeb.Admin.ForgesLive do
  @moduledoc """
  The stored forges (D39): GitHub Apps and Forgejo instances, never their secrets, with
  whether the fleet is running on them and why not when the forges and settings don't fit
  (`Forges.fleet_config/0`, `Fleet.Loader.status/0`). Links to the add flows; edit and remove.
  Refreshes on `{:forges_changed}`.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.Forges
  alias KuiperCoordinator.Fleet.Loader

  @impl true
  def mount(_params, _session, socket) do
    if connected?(socket), do: Forges.subscribe()
    {:ok, socket |> assign(page_title: "Forges") |> load()}
  end

  defp load(socket) do
    assign(socket,
      forges: Forges.list_forges(),
      settings: Forges.get_settings(),
      config: Forges.fleet_config(),
      loader: loader_status()
    )
  end

  defp loader_status do
    if Process.whereis(Loader), do: Loader.status(), else: :off
  catch
    :exit, _ -> :off
  end

  @impl true
  def handle_info({:forges_changed}, socket), do: {:noreply, load(socket)}

  @impl true
  def handle_event("delete", %{"id" => id}, socket) do
    socket =
      with {id, ""} <- Integer.parse(to_string(id)),
           %{} = forge <- Enum.find(socket.assigns.forges, &(&1.id == id)),
           {:ok, _} <- Forges.delete_forge(forge) do
        put_flash(socket, :info, "Removed #{forge.name}")
      else
        _ -> put_flash(socket, :error, "No such forge")
      end

    {:noreply, load(socket)}
  end

  defp mode(:fixed_capacity), do: "fixed capacity"
  defp mode(m), do: to_string(m)

  defp has_secret?(nil), do: false
  defp has_secret?(""), do: false
  defp has_secret?(_), do: true

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:forges}>
      <.header>
        Forges
        <:subtitle>
          The GitHub Apps and Forgejo instances runners are provisioned for. Every change
          reloads the fleet without a restart.
        </:subtitle>
        <:actions>
          <.link navigate={~p"/admin/forges/github/new"} class="btn btn-primary btn-sm">
            Add GitHub forge
          </.link>
          <.link navigate={~p"/admin/forges/forgejo/new"} class="btn btn-sm">
            Add Forgejo forge
          </.link>
          <.link navigate={~p"/admin/forges/provisioning"} class="btn btn-sm" id="provisioning-link">
            Provisioning
          </.link>
        </:actions>
      </.header>

      <section class="card bg-base-200 mb-4" id="fleet-state">
        <div class="card-body py-3">
          <p>
            Mode: <strong>{mode(@settings.mode)}</strong>
            <span :if={@settings.runner_scope}>· scope <code>{@settings.runner_scope}</code></span>
          </p>
          <p :if={match?({:error, _}, @config)} id="fleet-config-error" class="text-error">
            Fleet is off: {elem(@config, 1)}
          </p>
          <p :if={match?({:ok, %{fleet: nil}}, @config)} id="fleet-idle" class="opacity-70">
            No forges stored, so nothing is provisioned.
          </p>
          <p :if={match?({:running, _, _}, @loader)} id="fleet-running" class="text-success">
            Fleet running ({mode(elem(@loader, 1))}).
          </p>
          <p :if={match?({:error, _}, @loader)} id="fleet-loader-error" class="text-error">
            Fleet failed to start: {inspect(elem(@loader, 1))}
          </p>
        </div>
      </section>

      <p :if={@forges == []} class="opacity-70">
        No forges yet. Add a GitHub App or a Forgejo instance.
      </p>

      <table :if={@forges != []} class="table" id="forges">
        <thead>
          <tr>
            <th>Name</th><th>Type</th><th>Where</th><th>Webhook secret</th><th></th>
          </tr>
        </thead>
        <tbody>
          <tr :for={f <- @forges} id={"forge-#{f.id}"}>
            <td class="font-medium">{f.name}</td>
            <td>{if f.type == :github, do: "GitHub App", else: "Forgejo"}</td>
            <td :if={f.type == :github}>App ID <code>{f.app_id}</code></td>
            <td :if={f.type == :forgejo}>
              <code>{f.url}</code>
              <span class="opacity-70">· {Enum.join(f.scopes, ", ")}</span>
            </td>
            <td>{if has_secret?(f.webhook_secret), do: "set", else: "none"}</td>
            <td class="flex gap-1 justify-end">
              <.link navigate={~p"/admin/forges/#{f.id}/edit"} class="btn btn-xs">Edit</.link>
              <button
                id={"delete-forge-#{f.id}"}
                class="btn btn-error btn-xs"
                phx-click="delete"
                phx-value-id={f.id}
                data-confirm={"Remove #{f.name}? Its runners stop being provisioned."}
              >
                Remove
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </Layouts.admin>
    """
  end
end
