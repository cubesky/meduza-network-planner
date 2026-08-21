'use strict';
'require view';
'require form';
'require uci';

return view.extend({
	load: function() {
		return Promise.all([uci.load('meduza'), uci.load('firewall')]);
	},

	render: function() {
		var m = new form.Map('meduza', _('Meduza'),
			_('Etcd-managed routed side gateway. Every tinc, OpenVPN and WireGuard interface owned by Meduza uses the firewall zone selected here.'));
		var s = m.section(form.NamedSection, 'main', 'meduza', _('Settings'));
		s.addremove = false;

		var o = s.option(form.Flag, 'enable', _('Enable'));
		o.rmempty = false;

		o = s.option(form.Value, 'NODE_ID', _('Node ID'));
		o.rmempty = false;

		o = s.option(form.Value, 'ETCD_ENDPOINTS', _('Etcd endpoints'));
		o.placeholder = 'https://127.0.0.1:2379';
		o.rmempty = false;

		o = s.option(form.Value, 'ETCD_CA', _('Etcd CA certificate'));
		o.placeholder = '/etc/meduza/pki/ca.crt';

		o = s.option(form.Value, 'ETCD_CERT', _('Etcd client certificate'));
		o.placeholder = '/etc/meduza/pki/client.crt';

		o = s.option(form.Value, 'ETCD_KEY', _('Etcd client key'));
		o.placeholder = '/etc/meduza/pki/client.key';

		o = s.option(form.Value, 'ETCD_USER', _('Etcd user'));
		o = s.option(form.Value, 'ETCD_PASS', _('Etcd password'));
		o.password = true;

		o = s.option(form.ListValue, 'VPN_FIREWALL_ZONE', _('VPN firewall zone'));
		o.value('', _('No zone'));
		uci.sections('firewall', 'zone', function(z) {
			if (z.name)
				o.value(z.name, z.name);
		});
		o.description = _('Applied automatically to every tinc_*, ovpn_* and wg_* interface managed by Meduza. Existing OpenClash interfaces and rules are not changed.');

		return m.render();
	}
});
