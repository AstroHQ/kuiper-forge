defmodule KuiperCoordinatorWeb.Admin.ForgeFormLive do
  @moduledoc """
  Add a Forgejo forge, or edit any stored forge (D39), with its provisioning mode and what
  that mode needs. Saving runs `Forges`' live check and shows its message when the forge
  doesn't answer right. Secrets (private key, token, webhook
  secret) are never filled back in: left blank on edit, the stored one stays.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.Forges
  alias KuiperCoordinator.Forges.Forge

  @impl true
  def mount(params, _session, socket) do
    case forge(socket.assigns.live_action, params) do
      %Forge{} = forge ->
        {:ok,
         socket
         |> assign(
           forge: forge,
           page_title: if(forge.id, do: "Edit #{forge.name}", else: "Add Forgejo forge")
         )
         |> assign_form(Forges.change_forge(forge))}

      nil ->
        {:ok,
         socket |> put_flash(:error, "No such forge") |> push_navigate(to: ~p"/admin/forges")}
    end
  end

  defp forge(:new_forgejo, _), do: %Forge{type: :forgejo}

  defp forge(:edit, %{"id" => id}) do
    with {id, ""} <- Integer.parse(id),
         do: KuiperCoordinator.Repo.get(Forge, id),
         else: (_ -> nil)
  end

  # secrets never go back to the browser; scopes are edited one per line
  defp assign_form(socket, cs, scopes_text \\ nil) do
    cs =
      Enum.reduce(Forge.secret_fields(), cs, fn f, cs ->
        %{cs | data: Map.put(cs.data, f, nil), changes: Map.delete(cs.changes, f)}
      end)

    scopes_text =
      scopes_text || Enum.join(Ecto.Changeset.get_field(cs, :scopes) || [], "\n")

    assign(socket, form: to_form(cs, as: :forge), scopes_text: scopes_text)
  end

  @impl true
  def handle_event("save", %{"forge" => params}, socket) when is_map(params) do
    forge = socket.assigns.forge
    attrs = attrs(params, forge)
    opts = [req_options: Application.get_env(:kuiper_coordinator, :forge_req_options, [])]

    result =
      if forge.id,
        do: Forges.update_forge(forge, attrs, opts),
        else: Forges.create_forge(attrs, opts)

    case result do
      {:ok, saved} ->
        {:noreply,
         socket
         |> put_flash(:info, "Saved #{saved.name}")
         |> push_navigate(to: ~p"/admin/forges")}

      {:error, %Ecto.Changeset{} = cs} ->
        {:noreply, assign_form(socket, cs, str(params["scopes"]))}
    end
  end

  def handle_event("save", _params, socket), do: {:noreply, socket}

  @fields ~w(name app_id private_key url token webhook_secret mode runner_scope poll_interval_s)

  defp attrs(params, forge) do
    base =
      params
      |> Map.take(@fields)
      |> Map.filter(fn {_, v} -> is_binary(v) end)

    base =
      case params["scopes"] do
        s when is_binary(s) -> Map.put(base, "scopes", String.split(s, ["\n", ","], trim: true))
        _ -> base
      end

    if forge.id, do: base, else: Map.put(base, "type", to_string(forge.type))
  end

  defp str(v) when is_binary(v), do: v
  defp str(_), do: nil

  # fixed capacity fills pools with registration-token runners, which Forgejo doesn't have
  defp mode_options(:github),
    do: [
      {"Webhook", "webhook"},
      {"Polling", "polling"},
      {"Fixed capacity (pools)", "fixed_capacity"}
    ]

  defp mode_options(_type), do: [{"Webhook", "webhook"}, {"Polling", "polling"}]

  defp base_errors(form) do
    for {msg, _} <- Keyword.get_values(form.source.errors, :base), do: msg
  end

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:forges}>
      <.header>
        {@page_title}
        <:subtitle :if={@forge.type == :forgejo}>
          A Forgejo instance and the orgs, repos or users (<code>org</code>, <code>owner/repo</code>, <code>@user</code>, or <code>*</code>) runners are made for.
          The token is checked against each scope when you save.
        </:subtitle>
        <:subtitle :if={@forge.type == :github}>
          A GitHub App. Its ID and key came from the Add GitHub forge flow; paste a new key here
          to rotate it.
        </:subtitle>
      </.header>

      <div
        :for={msg <- base_errors(@form)}
        id="forge-base-error"
        role="alert"
        class="alert alert-error mb-4"
      >
        {msg}
      </div>

      <.form for={@form} id="forge-form" phx-submit="save" class="max-w-xl space-y-2">
        <.input field={@form[:name]} label="Name (no spaces or ':')" />
        <%= if @forge.type == :github do %>
          <.input field={@form[:app_id]} type="number" label="App ID" />
          <.input
            field={@form[:private_key]}
            type="textarea"
            label={secret_label("Private key (PEM)", @forge)}
          />
        <% else %>
          <.input field={@form[:url]} label="URL" placeholder="https://git.example.com" />
          <.input
            field={@form[:token]}
            type="password"
            label={secret_label("Token", @forge)}
            autocomplete="off"
          />
          <.input
            name="forge[scopes]"
            id="forge_scopes"
            type="textarea"
            label="Scopes, one per line"
            value={@scopes_text}
            errors={Enum.map(Keyword.get_values(@form.source.errors, :scopes), &translate_error/1)}
          />
        <% end %>
        <.input
          field={@form[:mode]}
          type="select"
          label="Mode: how jobs on this forge get runners"
          options={mode_options(@forge.type)}
        />
        <.input
          field={@form[:webhook_secret]}
          type="password"
          label={secret_label("Webhook secret (webhook mode)", @forge)}
          autocomplete="off"
        />
        <.input
          field={@form[:poll_interval_s]}
          type="number"
          label="Poll interval, seconds (polling mode, at least 10)"
        />
        <.input
          :if={@forge.type == :github}
          field={@form[:runner_scope]}
          label="Runner scope (fixed capacity), like org or owner/repo"
        />
        <div class="flex gap-2">
          <.button class="btn btn-primary">Save</.button>
          <.link navigate={~p"/admin/forges"} class="btn">Cancel</.link>
        </div>
      </.form>
    </Layouts.admin>
    """
  end

  defp secret_label(label, %Forge{id: nil}), do: label
  defp secret_label(label, _), do: label <> " (leave blank to keep the stored one)"
end
