from invoke import Collection

from . import docker, elbencho, mlperf

ns = Collection()
ns.add_collection(docker.ns)
ns.add_collection(elbencho.ns)
ns.add_collection(mlperf.ns)
