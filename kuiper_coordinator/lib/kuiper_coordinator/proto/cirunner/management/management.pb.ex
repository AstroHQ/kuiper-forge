defmodule Cirunner.Management.CreateTokenRequest do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.CreateTokenRequest",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :expires_secs, 1, type: :uint64, json_name: "expiresSecs"
  field :created_by, 2, type: :string, json_name: "createdBy"
end

defmodule Cirunner.Management.CreateTokenResponse do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.CreateTokenResponse",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :token, 1, type: :string
  field :expires_at, 2, type: :string, json_name: "expiresAt"
  field :created_at, 3, type: :string, json_name: "createdAt"
  field :server_ca_pem, 4, type: :string, json_name: "serverCaPem"
  field :server_trust_mode, 5, type: :string, json_name: "serverTrustMode"
end

defmodule Cirunner.Management.ListTokensRequest do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.ListTokensRequest",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3
end

defmodule Cirunner.Management.ListTokensResponse do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.ListTokensResponse",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :tokens, 1, repeated: true, type: Cirunner.Management.TokenInfo
end

defmodule Cirunner.Management.TokenInfo do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.TokenInfo",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :token, 1, type: :string
  field :expires_at, 2, type: :string, json_name: "expiresAt"
  field :created_by, 3, type: :string, json_name: "createdBy"
  field :created_at, 4, type: :string, json_name: "createdAt"
end

defmodule Cirunner.Management.DeleteTokenRequest do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.DeleteTokenRequest",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :token, 1, type: :string
end

defmodule Cirunner.Management.DeleteTokenResponse do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.DeleteTokenResponse",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :deleted, 1, type: :bool
end

defmodule Cirunner.Management.ListAgentsRequest do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.ListAgentsRequest",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3
end

defmodule Cirunner.Management.ListAgentsResponse do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.ListAgentsResponse",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :agents, 1, repeated: true, type: Cirunner.Management.AgentInfo
end

defmodule Cirunner.Management.AgentInfo do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.AgentInfo",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :agent_id, 1, type: :string, json_name: "agentId"
  field :hostname, 2, type: :string
  field :agent_type, 3, type: :string, json_name: "agentType"
  field :labels, 4, repeated: true, type: :string
  field :max_vms, 5, type: :uint32, json_name: "maxVms"
  field :created_at, 6, type: :string, json_name: "createdAt"
  field :expires_at, 7, type: :string, json_name: "expiresAt"
  field :revoked, 8, type: :bool
  field :agent_version, 9, type: :string, json_name: "agentVersion"
end

defmodule Cirunner.Management.RevokeAgentRequest do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.RevokeAgentRequest",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :agent_id, 1, type: :string, json_name: "agentId"
end

defmodule Cirunner.Management.RevokeAgentResponse do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.management.RevokeAgentResponse",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :revoked, 1, type: :bool
end

defmodule Cirunner.Management.ManagementService.Service do
  @moduledoc false

  use GRPC.Service,
    name: "cirunner.management.ManagementService",
    protoc_gen_elixir_version: "0.17.1"

  rpc(
    :CreateToken,
    Cirunner.Management.CreateTokenRequest,
    Cirunner.Management.CreateTokenResponse
  )

  rpc(:ListTokens, Cirunner.Management.ListTokensRequest, Cirunner.Management.ListTokensResponse)

  rpc(
    :DeleteToken,
    Cirunner.Management.DeleteTokenRequest,
    Cirunner.Management.DeleteTokenResponse
  )

  rpc(:ListAgents, Cirunner.Management.ListAgentsRequest, Cirunner.Management.ListAgentsResponse)

  rpc(
    :RevokeAgent,
    Cirunner.Management.RevokeAgentRequest,
    Cirunner.Management.RevokeAgentResponse
  )
end

defmodule Cirunner.Management.ManagementService.Stub do
  @moduledoc false

  use GRPC.Stub, service: Cirunner.Management.ManagementService.Service
end
