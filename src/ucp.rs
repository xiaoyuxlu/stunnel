use std::net::{ UdpSocket, SocketAddr };
use std::collections::{ VecDeque, HashMap };
use std::cell::RefCell;
use std::io::Error;
use std::rc::Rc;
use std::str::FromStr;
use std::time::Duration;
use std::vec::Vec;
use crc::crc32;
use rand::random;
use time::{ Timespec, get_time };

const CMD_SYN: u8 = 128;
const CMD_SYN_ACK: u8 = 129;
const CMD_ACK: u8 = 130;
const CMD_DATA: u8 = 131;
const CMD_HEARTBEAT: u8 = 132;
const CMD_HEARTBEAT_ACK: u8 = 133;
const UCP_PACKET_META_SIZE: usize = 29;
const DEFAULT_WINDOW: u32 = 256;
const DEFAULT_RTO: u32 = 100;


struct UcpPacket {
    buf: [u8; 1400],
    size: usize,
    payload: u16,

    session_id: u32,
    timestamp: u32,
    window: u32,
    xmit: u32,
    una: u32,
    seq: u32,
    cmd: u8,
}

impl UcpPacket {
    fn new() -> UcpPacket {
        UcpPacket {
            buf: [0; 1400],
            size: 0,
            payload: 0,
            session_id: 0,
            timestamp: 0,
            window: 0,
            xmit: 0,
            una: 0,
            seq: 0,
            cmd: 0
        }
    }

    fn parse(&mut self) -> bool {
        if !self.is_legal() {
            return false
        }

        self.payload = (self.size - UCP_PACKET_META_SIZE) as u16;

        let mut offset = 4;
        self.session_id = self.parse_u32(&mut offset);
        self.timestamp = self.parse_u32(&mut offset);
        self.window = self.parse_u32(&mut offset);
        self.xmit = self.parse_u32(&mut offset);
        self.una = self.parse_u32(&mut offset);
        self.seq = self.parse_u32(&mut offset);
        self.cmd = self.parse_u8(&mut offset);

        self.cmd >= CMD_SYN && self.cmd <= CMD_HEARTBEAT_ACK
    }

    fn pack(&mut self) {
        let mut offset = 4;
        let session_id = self.session_id;
        let timestamp = self.timestamp;
        let window = self.window;
        let xmit = self.xmit;
        let una = self.una;
        let seq = self.seq;
        let cmd = self.cmd;

        self.write_u32(&mut offset, session_id);
        self.write_u32(&mut offset, timestamp);
        self.write_u32(&mut offset, window);
        self.write_u32(&mut offset, xmit);
        self.write_u32(&mut offset, una);
        self.write_u32(&mut offset, seq);
        self.write_u8(&mut offset, cmd);

        offset = 0;
        let digest = crc32::checksum_ieee(&self.buf[4..self.size]);
        self.write_u32(&mut offset, digest);
        self.size = self.payload as usize + UCP_PACKET_META_SIZE;
    }

    fn packed_buffer(&self) -> &[u8] {
        &self.buf[..self.size]
    }

    fn parse_u32(&self, offset: &mut isize) -> u32 {
        let u = unsafe {
            *(self.buf.as_ptr().offset(*offset) as *const u32)
        };

        *offset += 4;
        u32::from_be(u)
    }

    fn parse_u8(&self, offset: &mut isize) -> u8 {
        let u = self.buf[*offset as usize];
        *offset += 1;
        u
    }

    fn write_u32(&mut self, offset: &mut isize, u: u32) {
        unsafe {
            *(self.buf.as_ptr().offset(*offset) as *mut u32)
                = u.to_be();
        }

        *offset += 4;
    }

    fn write_u8(&mut self, offset: &mut isize, u: u8) {
        self.buf[*offset as usize] = u;
        *offset += 1;
    }

    fn is_legal(&self) -> bool {
        self.size > UCP_PACKET_META_SIZE && self.is_crc32_correct()
    }

    fn is_crc32_correct(&self) -> bool {
        let mut offset = 0;
        let digest = self.parse_u32(&mut offset);
        crc32::checksum_ieee(&self.buf[4..self.size]) == digest
    }

    fn is_syn(&self) -> bool {
        self.cmd == CMD_SYN
    }

    fn remaining_load(&self) -> usize {
        self.buf.len() - self.payload as usize - UCP_PACKET_META_SIZE
    }

    fn payload_start(&self) -> isize {
        UCP_PACKET_META_SIZE as isize
    }

    fn payload_offset(&self) -> isize {
        (self.payload as usize + UCP_PACKET_META_SIZE) as isize
    }

    fn payload_write_u32(&mut self, u: u32) -> bool {
        if self.remaining_load() >= 4 {
            let mut offset = self.payload_offset();
            self.write_u32(&mut offset, u);
            self.payload += 4;
            true
        } else {
            false
        }
    }

    fn payload_write_slice(&mut self, buf: &[u8]) -> bool {
        if self.remaining_load() >= buf.len() {
            let offset = self.payload_offset() as usize;
            let end = offset + buf.len();
            self.buf[offset..end].copy_from_slice(buf);
            true
        } else {
            false
        }
    }
}

type UcpPacketQueue = VecDeque<Box<UcpPacket>>;

enum UcpState {
    NONE,
    ACCEPTING,
    CONNECTING,
    ESTABLISHED
}

struct UcpStreamImpl {
    socket: UdpSocket,
    remote_addr: SocketAddr,
    initial_time: Timespec,
    state: UcpState,

    send_queue: UcpPacketQueue,
    recv_queue: UcpPacketQueue,
    send_buffer: UcpPacketQueue,

    ack_list: Vec<u32>,
    session_id: u32,
    local_window: u32,
    remote_window: u32,
    seq: u32,
    una: u32,
    rto: u32,

    on_update: Option<Box<FnMut ()>>,
    on_readable: Option<Box<FnMut ()>>
}

impl UcpStreamImpl {
    fn new(socket: UdpSocket, remote_addr: SocketAddr) -> UcpStreamImpl {
        UcpStreamImpl {
            socket: socket,
            remote_addr: remote_addr,
            initial_time: get_time(),
            state: UcpState::NONE,

            send_queue: UcpPacketQueue::new(),
            recv_queue: UcpPacketQueue::new(),
            send_buffer: UcpPacketQueue::new(),

            ack_list: Vec::new(),
            local_window: DEFAULT_WINDOW,
            remote_window: DEFAULT_WINDOW,
            rto: DEFAULT_RTO,
            session_id: 0,
            seq: 0, una: 0,

            on_update: None,
            on_readable: None
        }
    }

    fn set_on_update<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut () {
        self.on_update = Some(Box::new(cb));
    }

    fn set_on_readable<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut () {
        self.on_readable = Some(Box::new(cb));
    }

    fn send(&self, buf: &[u8]) {

    }

    fn recv(&self, buf: &mut [u8]) -> usize {
        0
    }

    fn update(&self) {

    }

    fn process_packet(&mut self, packet: Box<UcpPacket>,
                      remote_addr: SocketAddr) {
        if self.remote_addr != remote_addr {
            return
        }

        match self.state {
            UcpState::NONE => if packet.is_syn() {
                self.accepting(packet);
            },
            _ => {
                self.processing(packet)
            }
        }
    }

    fn connecting(&mut self) {
        self.state = UcpState::CONNECTING;
        self.session_id = random::<u32>();

        let syn = self.new_packet(CMD_SYN);
        self.send_packet(syn);
    }

    fn accepting(&mut self, packet: Box<UcpPacket>) {
        self.state = UcpState::ACCEPTING;
        self.session_id = packet.session_id;
        self.remote_window = packet.window;

        let mut syn_ack = self.new_packet(CMD_SYN_ACK);
        syn_ack.payload_write_u32(packet.seq);
        syn_ack.payload_write_u32(packet.timestamp);
        self.send_packet(syn_ack);
    }

    fn processing(&mut self, packet: Box<UcpPacket>) {
        if self.session_id != packet.session_id {
            return
        }

        self.remote_window = packet.window;

        match self.state {
            UcpState::ACCEPTING => {
                self.process_state_accepting(packet);
            },
            UcpState::CONNECTING => {
                self.process_state_connecting(packet);
            },
            UcpState::ESTABLISHED => {
                self.process_state_established(packet);
            },
            UcpState::NONE => {}
        }
    }

    fn process_state_accepting(&mut self, packet: Box<UcpPacket>) {

    }

    fn process_state_connecting(&mut self, packet: Box<UcpPacket>) {
        if packet.cmd == CMD_SYN_ACK && packet.payload == 8 {
            let mut offset = packet.payload_start();
            let seq = packet.parse_u32(&mut offset);
            let timestamp = packet.parse_u32(&mut offset);

            if self.process_ack(seq, timestamp) {
                let mut ack = self.new_ack_packet();
                ack.payload_write_u32(packet.seq);
                ack.payload_write_u32(packet.timestamp);

                self.send_packet_directly(&mut ack);
                self.state = UcpState::ESTABLISHED;
            }
        }
    }

    fn process_state_established(&mut self, packet: Box<UcpPacket>) {

    }

    fn process_ack(&mut self, seq: u32, timestamp: u32) -> bool {
        for i in 0 .. self.send_queue.len() {
            if self.send_queue[i].seq == seq {
                let rtt = self.timestamp() - timestamp;
                self.rto = (self.rto + rtt) / 2;
                self.send_queue.remove(i);
                return true
            }
        }

        false
    }

    fn new_packet(&mut self, cmd: u8) -> Box<UcpPacket> {
        let mut packet = Box::new(UcpPacket::new());

        packet.session_id = self.session_id;
        packet.timestamp = self.timestamp();
        packet.window = self.local_window;
        packet.seq = self.next_seq();
        packet.una = self.una;
        packet.cmd = cmd;

        packet
    }

    fn new_ack_packet(&mut self) -> Box<UcpPacket> {
        let mut packet = Box::new(UcpPacket::new());

        packet.session_id = self.session_id;
        packet.timestamp = self.timestamp();
        packet.window = self.local_window;
        packet.una = self.una;
        packet.cmd = CMD_ACK;

        packet
    }

    fn timestamp(&self) -> u32 {
        (get_time() - self.initial_time).num_milliseconds() as u32
    }

    fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    fn send_packet(&mut self, mut packet: Box<UcpPacket>) {
        if self.send_queue.len() < self.remote_window as usize {
            self.send_packet_directly(&mut packet);
            self.send_queue.push_back(packet);
        } else {
            self.send_buffer.push_back(packet);
        }
    }

    fn send_packet_directly(&self, packet: &mut Box<UcpPacket>) {
        packet.pack();
        let _ = self.socket.send_to(packet.packed_buffer(), self.remote_addr);
    }
}

pub struct UcpStream {
    ucp_impl: Rc<RefCell<UcpStreamImpl>>
}

impl UcpStream {
    fn new(ucp_impl: Rc<RefCell<UcpStreamImpl>>) -> UcpStream {
        UcpStream { ucp_impl: ucp_impl }
    }

    pub fn set_on_update<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut () {
        self.ucp_impl.borrow_mut().set_on_update(cb);
    }

    pub fn set_on_readable<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut () {
        self.ucp_impl.borrow_mut().set_on_readable(cb);
    }

    pub fn send(&self, buf: &[u8]) {
        self.ucp_impl.borrow().send(buf);
    }

    pub fn recv(&self, buf: &mut [u8]) -> usize {
        self.ucp_impl.borrow().recv(buf)
    }
}

pub struct UcpClient {
    socket: UdpSocket,
    ucp: UcpStreamImpl,
    update_time: Timespec
}

impl UcpClient {
    pub fn connect(server_addr: &str) -> UcpClient {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let remote_addr = SocketAddr::from_str(server_addr).unwrap();

        let socket2 = socket.try_clone().unwrap();
        let mut ucp = UcpStreamImpl::new(socket2, remote_addr);
        ucp.connecting();

        socket.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        UcpClient { socket: socket, ucp: ucp, update_time: get_time() }
    }

    pub fn set_on_update<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut () {
        self.ucp.set_on_update(cb);
    }

    pub fn set_on_readable<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut () {
        self.ucp.set_on_readable(cb);
    }

    pub fn run(&mut self) {
        loop {
            let mut packet = Box::new(UcpPacket::new());
            let result = self.socket.recv_from(&mut packet.buf);

            if let Ok((size, remote_addr)) = result {
                packet.size = size;
                self.process_packet(packet, remote_addr);
            }

            self.update();
        }
    }

    pub fn send(&self, buf: &[u8]) {
        self.ucp.send(buf);
    }

    pub fn recv(&self, buf: &mut [u8]) -> usize {
        self.ucp.recv(buf)
    }

    fn update(&mut self) {
        let now = get_time();
        if (now - self.update_time).num_milliseconds() < 10 {
            return
        }

        self.ucp.update();
        self.update_time = now;
    }

    fn process_packet(&mut self, mut packet: Box<UcpPacket>,
                      remote_addr: SocketAddr) {
        if !packet.parse() {
            return
        }

        self.ucp.process_packet(packet, remote_addr);
    }
}

type UcpStreamMap = HashMap<SocketAddr, Rc<RefCell<UcpStreamImpl>>>;

pub struct UcpServer {
    socket: UdpSocket,
    ucp_map: UcpStreamMap,
    on_new_ucp: Option<Box<FnMut (UcpStream)>>,
    update_time: Timespec
}

impl UcpServer {
    pub fn listen(listen_addr: &str) -> Result<UcpServer, Error> {
        match UdpSocket::bind(listen_addr) {
            Ok(socket) => {
                socket.set_read_timeout(
                    Some(Duration::from_millis(10))).unwrap();
                Ok(UcpServer { socket: socket,
                    ucp_map: UcpStreamMap::new(),
                    on_new_ucp: None,
                    update_time: get_time() })
            },
            Err(e) => Err(e)
        }
    }

    pub fn set_on_new_ucp_stream<CB>(&mut self, cb: CB)
        where CB: 'static + FnMut (UcpStream) {
        self.on_new_ucp = Some(Box::new(cb));
    }

    pub fn run(&mut self) {
        loop {
            let mut packet = Box::new(UcpPacket::new());
            let result = self.socket.recv_from(&mut packet.buf);

            if let Ok((size, remote_addr)) = result {
                packet.size = size;
                self.process_packet(packet, remote_addr);
            }

            self.update();
        }
    }

    fn update(&mut self) {
        let now = get_time();
        if (now - self.update_time).num_milliseconds() < 10 {
            return
        }

        for (_, ucp) in self.ucp_map.iter() {
            ucp.borrow().update();
        }

        self.update_time = now;
    }

    fn process_packet(&mut self, mut packet: Box<UcpPacket>,
                      remote_addr: SocketAddr) {
        if !packet.parse() {
            return
        }

        if let Some(ucp) = self.ucp_map.get_mut(&remote_addr) {
            ucp.borrow_mut().process_packet(packet, remote_addr);
            return
        }

        if packet.is_syn() {
            self.new_ucp_stream(packet, remote_addr);
        }
    }

    fn new_ucp_stream(&mut self, packet: Box<UcpPacket>,
                      remote_addr: SocketAddr) {
        let socket = self.socket.try_clone().unwrap();
        let ucp_impl = Rc::new(RefCell::new(
                UcpStreamImpl::new(socket, remote_addr)));
        let ucp = UcpStream::new(ucp_impl.clone());

        if let Some(ref mut on_new_ucp) = self.on_new_ucp {
            on_new_ucp(ucp);
        }

        let _ = self.ucp_map.insert(remote_addr, ucp_impl.clone());
        ucp_impl.borrow_mut().process_packet(packet, remote_addr);
    }
}