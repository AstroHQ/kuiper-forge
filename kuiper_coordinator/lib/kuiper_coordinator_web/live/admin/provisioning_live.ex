defmodule KuiperCoordinatorWeb.Admin.ProvisioningLive do
  @moduledoc """
  What every forge's provisioning shares (D39): the one settings row (idle timeout, scan
  lookback, required labels, webhook path) and the label mappings that route jobs to a scope
  and group. Each forge's mode is on its own page. Everything is saved through
  `Forges`, whose broadcast makes `Fleet.Loader` restart the fleet; no reload step here.
  Lists (labels) are edited as comma-separated text.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.Forges
  alias KuiperCoordinator.Forges.LabelMapping

  @settings_fields ~w(idle_runner_timeout_s scan_lookback_hours webhook_path)
  @mapping_fields ~w(runner_scope runner_group forge position)

  @impl true
  def mount(_params, _session, socket) do
    if connected?(socket), do: Forges.subscribe()

    {:ok,
     socket
     |> assign(page_title: "Provisioning", forge_names: forge_names())
     |> assign_settings(Forges.change_settings(Forges.get_settings()))
     |> assign_mappings()
     |> new_mapping()}
  end

  defp forge_names, do: Enum.map(Forges.list_forges(), & &1.name)

  defp assign_settings(socket, cs), do: assign(socket, settings_form: to_form(cs, as: :settings))
  defp assign_mappings(socket), do: assign(socket, mappings: Forges.list_label_mappings())

  defp new_mapping(socket, mapping \\ %LabelMapping{}) do
    assign(socket,
      mapping: mapping,
      mapping_form: to_form(Forges.change_label_mapping(mapping), as: :mapping)
    )
  end

  @impl true
  def handle_info({:forges_changed}, socket),
    do: {:noreply, socket |> assign_mappings() |> assign(forge_names: forge_names())}

  def handle_info(_, socket), do: {:noreply, socket}

  @impl true
  def handle_event("save_settings", %{"settings" => params}, socket) when is_map(params) do
    attrs =
      params
      |> strings(@settings_fields)
      |> put_list(params, "required_labels")

    case Forges.update_settings(attrs) do
      {:ok, s} ->
        {:noreply,
         socket
         |> put_flash(:info, "Saved provisioning settings")
         |> assign_settings(Forges.change_settings(s))}

      {:error, cs} ->
        {:noreply, assign_settings(socket, cs)}
    end
  end

  def handle_event("save_mapping", %{"mapping" => params}, socket) when is_map(params) do
    attrs = params |> strings(@mapping_fields) |> put_list(params, "labels")
    m = socket.assigns.mapping

    result =
      if m.id,
        do: Forges.update_label_mapping(m, attrs),
        else: Forges.create_label_mapping(attrs)

    case result do
      {:ok, _} ->
        {:noreply,
         socket |> put_flash(:info, "Saved label mapping") |> assign_mappings() |> new_mapping()}

      {:error, cs} ->
        {:noreply, assign(socket, mapping_form: to_form(cs, as: :mapping))}
    end
  end

  def handle_event("edit_mapping", %{"id" => id}, socket) do
    case find_mapping(socket, id) do
      nil -> {:noreply, socket}
      m -> {:noreply, new_mapping(socket, m)}
    end
  end

  def handle_event("cancel_mapping", _, socket), do: {:noreply, new_mapping(socket)}

  def handle_event("delete_mapping", %{"id" => id}, socket) do
    case find_mapping(socket, id) do
      nil ->
        {:noreply, socket}

      m ->
        _ = Forges.delete_label_mapping(m)
        socket = if socket.assigns.mapping.id == m.id, do: new_mapping(socket), else: socket
        {:noreply, socket |> put_flash(:info, "Removed label mapping") |> assign_mappings()}
    end
  end

  def handle_event(_, _, socket), do: {:noreply, socket}

  defp find_mapping(socket, id) when is_binary(id) do
    with {id, ""} <- Integer.parse(id),
         do: Enum.find(socket.assigns.mappings, &(&1.id == id)),
         else: (_ -> nil)
  end

  defp find_mapping(_, _), do: nil

  defp strings(params, fields),
    do: params |> Map.take(fields) |> Map.filter(fn {_, v} -> is_binary(v) end)

  defp put_list(attrs, params, key) do
    case params[key] do
      s when is_binary(s) -> Map.put(attrs, key, String.split(s, ",", trim: true))
      _ -> attrs
    end
  end

  defp list_text(form, field) do
    case form[field].value do
      l when is_list(l) -> Enum.join(l, ", ")
      s when is_binary(s) -> s
      _ -> ""
    end
  end

  defp field_errors(form, field),
    do: Enum.map(Keyword.get_values(form.source.errors, field), &translate_error/1)

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:forges}>
      <.header>
        Provisioning
        <:subtitle>
          What every forge shares; a forge's mode is set on the forge itself. Saving restarts
          the fleet with the new settings; runners and queued jobs carry over.
        </:subtitle>
        <:actions>
          <.link navigate={~p"/admin/forges"} class="btn btn-sm">Forges</.link>
        </:actions>
      </.header>

      <.form
        for={@settings_form}
        id="settings-form"
        phx-submit="save_settings"
        class="max-w-xl space-y-2 mb-8"
      >
        <.input
          field={@settings_form[:idle_runner_timeout_s]}
          type="number"
          label="Idle runner timeout, seconds (0 off, at least 120)"
        />
        <.input
          field={@settings_form[:scan_lookback_hours]}
          type="number"
          label="Forge scan lookback, hours (0 scans every repository)"
        />
        <.input
          name="settings[required_labels]"
          id="settings_required_labels"
          label="Labels a job must carry, comma-separated"
          value={list_text(@settings_form, :required_labels)}
          errors={field_errors(@settings_form, :required_labels)}
        />
        <.input
          field={@settings_form[:webhook_path]}
          label="Webhook path (every forge in webhook mode delivers here)"
        />
        <.button class="btn btn-primary">Save settings</.button>
      </.form>

      <h2 class="text-lg font-semibold mb-2">Label mappings</h2>
      <p class="opacity-70 mb-2">
        Jobs carrying all of a mapping's labels get runners in its scope and group. The lowest
        position that matches wins.
      </p>
      <table :if={@mappings != []} class="table table-sm mb-4" id="mappings">
        <thead>
          <tr>
            <th>Pos</th><th>Labels</th><th>Scope</th><th>Group</th><th>Forge</th><th></th>
          </tr>
        </thead>
        <tbody>
          <tr :for={m <- @mappings} id={"mapping-#{m.id}"}>
            <td>{m.position}</td>
            <td>{Enum.join(m.labels, ", ")}</td>
            <td>{m.runner_scope}</td>
            <td>{m.runner_group}</td>
            <td>{m.forge || "any"}</td>
            <td class="text-right">
              <button class="btn btn-xs" phx-click="edit_mapping" phx-value-id={m.id}>Edit</button>
              <button
                class="btn btn-xs btn-error"
                phx-click="delete_mapping"
                phx-value-id={m.id}
                data-confirm="Remove this label mapping?"
              >
                Remove
              </button>
            </td>
          </tr>
        </tbody>
      </table>

      <.form
        for={@mapping_form}
        id="mapping-form"
        phx-submit="save_mapping"
        class="max-w-xl space-y-2"
      >
        <h3 class="font-semibold">{if @mapping.id, do: "Edit mapping", else: "Add mapping"}</h3>
        <.input
          name="mapping[labels]"
          id="mapping_labels"
          label="Labels, comma-separated"
          value={list_text(@mapping_form, :labels)}
          errors={field_errors(@mapping_form, :labels)}
        />
        <.input field={@mapping_form[:runner_scope]} label="Runner scope" />
        <.input field={@mapping_form[:runner_group]} label="Runner group" />
        <.input
          field={@mapping_form[:forge]}
          type="select"
          label="Forge (blank: any)"
          prompt=""
          options={@forge_names}
        />
        <.input field={@mapping_form[:position]} type="number" label="Position" />
        <div class="flex gap-2">
          <.button class="btn btn-primary">
            {if @mapping.id, do: "Save mapping", else: "Add mapping"}
          </.button>
          <button :if={@mapping.id} type="button" class="btn" phx-click="cancel_mapping">
            Cancel
          </button>
        </div>
      </.form>
    </Layouts.admin>
    """
  end
end
