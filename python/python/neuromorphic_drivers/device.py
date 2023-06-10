from .generated import enums, unions
from .neuromorphic_drivers import Device as ExtensionDevice
from . import status


class Device(ExtensionDevice):
    def __new__(cls, *args, **kwargs):
        return super().__new__(cls, *args, **kwargs)

    def name(self):
        return enums.Name(super().name())

    def speed(self):
        return enums.Speed(super().speed())

    def properties(self):
        return unions.name_to_properties(self.name())

    def __iter__(self):
        return self

    def __next__(self):
        (system_time, packet_status), packet = super().__next__()
        return (
            status.Status(
                system_time=system_time, packet=status.PacketStatus(*packet_status)
            ),
            packet,
        )
