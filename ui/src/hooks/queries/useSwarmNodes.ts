import { useQuery } from '@tanstack/react-query';
import { api } from '../../lib/api-client';

export interface SwarmNode {
  id: number;
  addr: string;
  public_key: string;
  wireguard_key?: string;
  wireguard_port?: number;
  subnet_id?: number;
  is_leader: boolean;
}

export function useSwarmNodes() {
  return useQuery({
    queryKey: ['swarm', 'nodes'],
    queryFn: () => api.get<SwarmNode[]>('/swarm/nodes'),
  });
}
